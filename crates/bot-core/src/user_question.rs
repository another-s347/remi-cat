use std::collections::HashMap;
use std::sync::Arc;

use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    AgentError, ResumePayload, Tool, ToolContext, ToolOutput, ToolResult,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionOption {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestionRequest {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub tool_call_id: String,
    pub question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<UserQuestionOption>,
    #[serde(default)]
    pub allow_free_text: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_option_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserQuestionStatus {
    Answered,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestionResponse {
    pub question_id: String,
    pub status: UserQuestionStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_option_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug)]
struct PendingUserQuestion {
    request: UserQuestionRequest,
    tx: oneshot::Sender<UserQuestionResponse>,
}

#[derive(Debug, Default)]
pub struct UserQuestionManager {
    pending: Mutex<HashMap<String, PendingUserQuestion>>,
}

impl UserQuestionManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub async fn start(
        &self,
        request: UserQuestionRequest,
    ) -> oneshot::Receiver<UserQuestionResponse> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.lock().await;
        pending.insert(request.id.clone(), PendingUserQuestion { request, tx });
        rx
    }

    pub async fn answer(
        &self,
        question_id: &str,
        mut response: UserQuestionResponse,
    ) -> Option<UserQuestionRequest> {
        let pending = {
            let mut pending = self.pending.lock().await;
            pending.remove(question_id)
        }?;
        response.question_id = pending.request.id.clone();
        if response.answered_at.is_none() {
            response.answered_at = Some(chrono::Utc::now().to_rfc3339());
        }
        let _ = pending.tx.send(response.clone());
        tracing::info!(
            question_id,
            session_id = %pending.request.session_id,
            status = ?response.status,
            source = response.source.as_deref().unwrap_or(""),
            "user_question.answer"
        );
        Some(pending.request)
    }

    pub async fn cancel(
        &self,
        question_id: &str,
        source: Option<String>,
    ) -> Option<UserQuestionRequest> {
        self.answer(
            question_id,
            UserQuestionResponse {
                question_id: question_id.to_string(),
                status: UserQuestionStatus::Cancelled,
                selected_option_ids: Vec::new(),
                free_text: None,
                answer_text: Some("User cancelled the question.".to_string()),
                answered_at: None,
                source,
            },
        )
        .await
    }
}

#[derive(Debug, Clone)]
pub enum UserQuestionEvent {
    Requested(UserQuestionRequest),
    Updated(UserQuestionRequest),
    Resolved {
        request: UserQuestionRequest,
        response: UserQuestionResponse,
    },
}

#[derive(Clone)]
pub struct AskUserQuestionTool {
    manager: Arc<UserQuestionManager>,
}

impl AskUserQuestionTool {
    pub fn new(manager: Arc<UserQuestionManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct AskUserQuestionArgs {
    question: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    options: Vec<UserQuestionOption>,
    #[serde(default = "default_allow_free_text")]
    allow_free_text: bool,
    #[serde(default)]
    placeholder: Option<String>,
    #[serde(default)]
    default_option_id: Option<String>,
}

fn default_allow_free_text() -> bool {
    true
}

impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "ask_user_question"
    }

    fn description(&self) -> &str {
        "Ask the user a clarification question and wait for their answer. Use this when progress depends on user intent, missing information, or a choice that should not be guessed. Supports structured options and optional free-text details."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The concise question to show to the user."
                },
                "reason": {
                    "type": "string",
                    "description": "Optional short reason explaining why the answer is needed."
                },
                "options": {
                    "type": "array",
                    "description": "Optional structured choices. IDs must be unique and stable.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "label": { "type": "string" },
                            "description": { "type": "string" }
                        },
                        "required": ["id", "label"]
                    }
                },
                "allow_free_text": {
                    "type": "boolean",
                    "description": "Whether the user may provide arbitrary text in addition to or instead of choosing an option. Defaults to true."
                },
                "placeholder": {
                    "type": "string",
                    "description": "Optional placeholder for the free-text answer."
                },
                "default_option_id": {
                    "type": "string",
                    "description": "Optional recommended option id."
                }
            },
            "required": ["question"]
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let manager = Arc::clone(&self.manager);
        let thread_id = ctx.thread_id.clone();
        let run_id = ctx.run_id.clone();
        async move {
            let args: AskUserQuestionArgs = serde_json::from_value(arguments)
                .map_err(|err| AgentError::tool("ask_user_question", err.to_string()))?;
            let question = args.question.trim().to_string();
            if question.is_empty() {
                return Err(AgentError::tool(
                    "ask_user_question",
                    "question must not be empty",
                ));
            }
            validate_question_args(&args)?;
            let session_id = thread_id
                .as_ref()
                .map(|id| id.0.clone())
                .unwrap_or_else(|| "unknown".to_string());
            let run_id_value = run_id.0.clone();
            let request = UserQuestionRequest {
                id: format!("uq-{}", uuid::Uuid::new_v4()),
                session_id: session_id.clone(),
                run_id: run_id_value,
                tool_call_id: String::new(),
                question,
                reason: args
                    .reason
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                options: args.options,
                allow_free_text: args.allow_free_text,
                placeholder: args
                    .placeholder
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                default_option_id: args.default_option_id,
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            let rx = manager.start(request.clone()).await;
            Ok(ToolResult::Output(
                stream! {
                    yield crate::user_question_requested_marker(&request);
                    yield crate::user_question_updated_marker(&request);
                    let response = rx.await.unwrap_or_else(|_| UserQuestionResponse {
                        question_id: request.id.clone(),
                        status: UserQuestionStatus::Cancelled,
                        selected_option_ids: Vec::new(),
                        free_text: None,
                        answer_text: Some("Question was cancelled because the run ended.".to_string()),
                        answered_at: Some(chrono::Utc::now().to_rfc3339()),
                        source: Some("runtime".to_string()),
                    });
                    yield crate::user_question_resolved_marker(&request, &response);
                    let value = serde_json::to_string(&response)
                        .unwrap_or_else(|_| "{\"status\":\"cancelled\"}".to_string());
                    yield ToolOutput::text(value);
                }
                .boxed(),
            ))
        }
    }
}

fn validate_question_args(args: &AskUserQuestionArgs) -> Result<(), AgentError> {
    let mut ids = std::collections::HashSet::new();
    for option in &args.options {
        let id = option.id.trim();
        if id.is_empty() {
            return Err(AgentError::tool(
                "ask_user_question",
                "option id must not be empty",
            ));
        }
        if !ids.insert(id.to_string()) {
            return Err(AgentError::tool(
                "ask_user_question",
                format!("duplicate option id `{id}`"),
            ));
        }
        if option.label.trim().is_empty() {
            return Err(AgentError::tool(
                "ask_user_question",
                format!("option `{id}` label must not be empty"),
            ));
        }
    }
    if let Some(default_id) = args.default_option_id.as_deref() {
        if !ids.contains(default_id) {
            return Err(AgentError::tool(
                "ask_user_question",
                format!("default_option_id `{default_id}` does not match any option"),
            ));
        }
    }
    if args.options.is_empty() && !args.allow_free_text {
        return Err(AgentError::tool(
            "ask_user_question",
            "at least one option or allow_free_text=true is required",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use remi_agentloop::prelude::AgentConfig;
    use std::sync::RwLock;

    fn tool_context() -> ToolContext {
        ToolContext {
            config: AgentConfig::default(),
            thread_id: Some(remi_agentloop::prelude::ThreadId("thread-1".to_string())),
            run_id: remi_agentloop::prelude::RunId("run-1".to_string()),
            metadata: None,
            user_state: Arc::new(RwLock::new(serde_json::Value::Null)),
        }
    }

    #[tokio::test]
    async fn manager_answers_pending_question() {
        let manager = UserQuestionManager::new();
        let request = UserQuestionRequest {
            id: "q1".to_string(),
            session_id: "s1".to_string(),
            run_id: "r1".to_string(),
            tool_call_id: "call1".to_string(),
            question: "Pick one?".to_string(),
            reason: None,
            options: Vec::new(),
            allow_free_text: true,
            placeholder: None,
            default_option_id: None,
            created_at: "now".to_string(),
        };
        let rx = manager.start(request.clone()).await;
        let resolved = manager
            .answer(
                "q1",
                UserQuestionResponse {
                    question_id: "q1".to_string(),
                    status: UserQuestionStatus::Answered,
                    selected_option_ids: vec!["a".to_string()],
                    free_text: Some("detail".to_string()),
                    answer_text: Some("answer".to_string()),
                    answered_at: None,
                    source: Some("test".to_string()),
                },
            )
            .await;
        assert_eq!(resolved.expect("request should resolve").id, request.id);
        let response = rx.await.expect("response should be delivered");
        assert_eq!(response.status, UserQuestionStatus::Answered);
        assert_eq!(response.selected_option_ids, vec!["a"]);
        assert_eq!(response.free_text.as_deref(), Some("detail"));
        assert!(response.answered_at.is_some());
    }

    #[test]
    fn validate_rejects_duplicate_option_ids() {
        let err = validate_question_args(&AskUserQuestionArgs {
            question: "Choose".to_string(),
            reason: None,
            options: vec![
                UserQuestionOption {
                    id: "same".to_string(),
                    label: "One".to_string(),
                    description: None,
                },
                UserQuestionOption {
                    id: "same".to_string(),
                    label: "Two".to_string(),
                    description: None,
                },
            ],
            allow_free_text: true,
            placeholder: None,
            default_option_id: None,
        })
        .expect_err("duplicate ids should fail");
        assert!(err.to_string().contains("duplicate option id"));
    }

    #[tokio::test]
    async fn tool_waits_and_returns_answer_json() {
        let manager = UserQuestionManager::new();
        let tool = AskUserQuestionTool::new(Arc::clone(&manager));
        let ctx = tool_context();
        let result = tool
            .execute(
                serde_json::json!({
                    "question": "Continue?",
                    "options": [{"id": "yes", "label": "Yes"}]
                }),
                None,
                &ctx,
            )
            .await
            .expect("tool should start");
        let ToolResult::Output(stream) = result else {
            panic!("ask_user_question should output a stream");
        };
        let mut stream = std::pin::pin!(stream);
        let first = stream.next().await.expect("request marker");
        let first_event = match first {
            ToolOutput::SubSession(event) => event,
            _ => panic!("expected marker"),
        };
        let request = match crate::cat_event_from_subagent_approval_marker(&first_event) {
            Some(crate::CatEvent::UserQuestionRequested(request)) => request,
            other => panic!("unexpected event: {other:?}"),
        };
        let _updated = stream.next().await.expect("updated marker");
        manager
            .answer(
                &request.id,
                UserQuestionResponse {
                    question_id: request.id.clone(),
                    status: UserQuestionStatus::Answered,
                    selected_option_ids: vec!["yes".to_string()],
                    free_text: None,
                    answer_text: Some("Selected option ids: yes".to_string()),
                    answered_at: None,
                    source: Some("test".to_string()),
                },
            )
            .await;
        let _resolved = stream.next().await.expect("resolved marker");
        let output = stream.next().await.expect("tool result");
        let ToolOutput::Result(content) = output else {
            panic!("expected final result");
        };
        let value: serde_json::Value =
            serde_json::from_str(&content.text_content()).expect("json response");
        assert_eq!(value["status"], "answered");
        assert_eq!(value["selected_option_ids"][0], "yes");
    }
}
