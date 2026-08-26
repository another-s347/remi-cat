use std::time::Duration;

use a2a::{Message, Part, PartContent, Role, SendMessageRequest, StreamResponse, TaskState};
use futures::{StreamExt, TryStreamExt};
use remi_agentloop::prelude::CancellationToken;
use serde_json::Value;
use tokio::sync::mpsc;

pub(crate) const ACTIVITY_ARTIFACT_ID: &str = "remi-activity";
pub(crate) const ACTIVITY_EXTENSION: &str = "urn:remi:a2a:activity-stream:v1";
pub(crate) const INTERACTIVE_EXTENSION: &str = "urn:remi:a2a:interactive-input:v1";
pub(crate) const HANDOFF_EXTENSION: &str = "urn:remi:a2a:handoff:v1";
pub(crate) const INVOCATION_EXTENSION: &str = "urn:remi:a2a:invocation-context:v1";

#[derive(Debug)]
pub enum DelegateWireEvent {
    TaskAssigned {
        task_id: String,
        context_id: String,
    },
    Activity(Value),
    Terminal {
        state: TaskState,
        message: Option<String>,
    },
}

#[async_trait::async_trait]
pub trait A2aDelegateTransport: Send + Sync {
    async fn invoke(
        &self,
        request: SendMessageRequest,
    ) -> Result<mpsc::Receiver<Result<StreamResponse, String>>, String>;
    async fn cancel(&self, task_id: &str) -> Result<(), String>;
    async fn steer(&self, task_id: &str, content: bot_runtime_core::Content) -> Result<(), String>;
    async fn decide_approval(
        &self,
        task_id: &str,
        approval_id: &str,
        decision: crate::ToolApprovalDecision,
    ) -> Result<(), String>;
    async fn answer_question(
        &self,
        task_id: &str,
        question_id: &str,
        response: crate::UserQuestionResponse,
    ) -> Result<(), String>;
}

#[derive(Clone)]
pub(crate) struct A2aDelegateClient {
    client: reqwest::Client,
    endpoint: String,
    token: Option<String>,
    memory: Option<std::sync::Arc<dyn A2aDelegateTransport>>,
}

impl A2aDelegateClient {
    #[cfg(test)]
    pub(crate) fn for_test(endpoint: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
            token: None,
            memory: None,
        }
    }

    pub(crate) fn from_env(
        agent_id: &str,
        memory: Option<std::sync::Arc<dyn A2aDelegateTransport>>,
    ) -> Result<Self, String> {
        let configured = std::env::var("REMI_A2A_DELEGATE_ENDPOINTS")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                serde_json::from_str::<std::collections::HashMap<String, Value>>(&value)
                    .map_err(|error| format!("invalid REMI_A2A_DELEGATE_ENDPOINTS: {error}"))
            })
            .transpose()?
            .and_then(|entries| entries.get(agent_id).cloned());
        let configured_endpoint = configured.as_ref().and_then(|value| {
            value.as_str().map(str::to_string).or_else(|| {
                value
                    .get("endpoint")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
        });
        let explicit_endpoint = configured_endpoint.or_else(|| {
            std::env::var("REMI_A2A_DELEGATE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
        if explicit_endpoint.is_none() {
            if let Some(memory) = memory {
                return Ok(Self {
                    client: reqwest::Client::new(),
                    endpoint: "memory://local-application".to_string(),
                    token: None,
                    memory: Some(memory),
                });
            }
        }
        let endpoint = explicit_endpoint
            .or_else(|| std::env::var("REMI_A2A_PUBLIC_URL").ok())
            .unwrap_or_else(|| {
                let host =
                    std::env::var("REMI_A2A_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
                let port = std::env::var("REMI_A2A_PORT").unwrap_or_else(|_| "8788".to_string());
                format!("http://{host}:{port}")
            });
        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            return Err("REMI_A2A_DELEGATE_URL must be an http(s) URL".to_string());
        }
        let parsed = reqwest::Url::parse(&endpoint)
            .map_err(|error| format!("invalid A2A delegate URL: {error}"))?;
        let loopback = parsed
            .host_str()
            .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
        if parsed.scheme() != "https" && !loopback {
            return Err("non-loopback A2A delegates require HTTPS".to_string());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|error| error.to_string())?;
        let token = configured
            .as_ref()
            .and_then(|value| value.get("token_env"))
            .and_then(Value::as_str)
            .and_then(|key| std::env::var(key).ok())
            .or_else(|| std::env::var("REMI_A2A_TOKEN").ok())
            .filter(|value| !value.trim().is_empty());
        Ok(Self {
            client,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            token,
            memory: None,
        })
    }

    async fn discover(&self) -> Result<(), String> {
        let response = self
            .client
            .get(format!("{}/.well-known/agent-card.json", self.endpoint))
            .send()
            .await
            .map_err(|error| format!("A2A Agent Card discovery failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "A2A Agent Card returned HTTP {}",
                response.status()
            ));
        }
        let card: a2a::AgentCard = response
            .json()
            .await
            .map_err(|error| format!("invalid A2A Agent Card: {error}"))?;
        if card.capabilities.streaming != Some(true) {
            return Err("A2A delegate does not advertise streaming".to_string());
        }
        let extensions = card.capabilities.extensions.unwrap_or_default();
        for required in [ACTIVITY_EXTENSION, INTERACTIVE_EXTENSION, HANDOFF_EXTENSION] {
            if !extensions.iter().any(|extension| extension.uri == required) {
                return Err(format!(
                    "A2A delegate is missing required extension {required}"
                ));
            }
        }
        Ok(())
    }

    pub(crate) async fn invoke<F>(
        &self,
        agent_id: &str,
        sub_session_id: &str,
        parent_thread_id: &str,
        task: String,
        cancel: CancellationToken,
        mut emit: F,
    ) -> Result<(), String>
    where
        F: FnMut(DelegateWireEvent) -> Result<(), String>,
    {
        let context_id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("remi:a2a:subsession:{sub_session_id}").as_bytes(),
        )
        .to_string();
        let request = SendMessageRequest {
            message: Message {
                message_id: uuid::Uuid::new_v4().to_string(),
                context_id: Some(context_id),
                task_id: None,
                role: Role::User,
                parts: vec![Part::text(task)],
                metadata: Some(std::collections::HashMap::from([(
                    INVOCATION_EXTENSION.to_string(),
                    serde_json::json!({
                        "agentId": agent_id,
                        "subSessionId": sub_session_id,
                        "parentThreadId": parent_thread_id,
                    }),
                )])),
                extensions: Some(vec![
                    ACTIVITY_EXTENSION.to_string(),
                    INTERACTIVE_EXTENSION.to_string(),
                    HANDOFF_EXTENSION.to_string(),
                    INVOCATION_EXTENSION.to_string(),
                ]),
                reference_task_ids: None,
            },
            configuration: None,
            metadata: None,
            tenant: None,
        };
        if let Some(memory) = &self.memory {
            let mut responses = memory.invoke(request).await?;
            let mut task_id = None;
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        if let Some(task_id) = task_id.as_deref() {
                            memory.cancel(task_id).await?;
                        }
                        return Err("A2A delegate task canceled".to_string());
                    }
                    response = responses.recv() => {
                        let Some(response) = response else { break; };
                        emit_stream_response(response?, &mut task_id, &mut emit)?;
                    }
                }
            }
            return Ok(());
        }
        self.discover().await?;
        let mut builder = self
            .client
            .post(format!("{}/message:stream", self.endpoint))
            .header("content-type", "application/a2a+json")
            .header("A2A-Version", "1.0")
            .header(
                "A2A-Extensions",
                [
                    ACTIVITY_EXTENSION,
                    INTERACTIVE_EXTENSION,
                    HANDOFF_EXTENSION,
                    INVOCATION_EXTENSION,
                ]
                .join(","),
            )
            .json(&request);
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }
        let response = builder.send().await.map_err(|error| {
            format!(
                "A2A delegate connection to {} failed: {error}",
                self.endpoint
            )
        })?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("A2A delegate returned HTTP {status}: {body}"));
        }

        let mut task_id: Option<String> = None;
        let mut buffer = String::new();
        let mut bytes = response.bytes_stream().map_err(|error| error.to_string());
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    if let Some(task_id) = task_id.as_deref() {
                        self.cancel(task_id).await?;
                    }
                    return Err("A2A delegate task canceled".to_string());
                }
                chunk = bytes.next() => {
                    let Some(chunk) = chunk else { break; };
                    let chunk = chunk?;
                    buffer.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(index) = buffer.find("\n\n") {
                        let frame = buffer[..index].to_string();
                        buffer.drain(..index + 2);
                        for line in frame.lines().filter_map(|line| line.strip_prefix("data:")) {
                            let response: StreamResponse = serde_json::from_str(line.trim())
                                .map_err(|error| format!("invalid A2A stream event: {error}"))?;
                            emit_stream_response(response, &mut task_id, &mut emit)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn cancel(&self, task_id: &str) -> Result<(), String> {
        if let Some(memory) = &self.memory {
            return memory.cancel(task_id).await;
        }
        let mut request = self
            .client
            .post(format!("{}/tasks/{task_id}:cancel", self.endpoint))
            .header("A2A-Version", "1.0");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("A2A cancel returned HTTP {}", response.status()))
        }
    }

    pub(crate) async fn steer(
        &self,
        task_id: &str,
        content: bot_runtime_core::Content,
    ) -> Result<(), String> {
        if let Some(memory) = &self.memory {
            return memory.steer(task_id, content).await;
        }
        let mut request = self
            .client
            .post(format!("{}/tasks/{task_id}/remi:steer", self.endpoint))
            .header("A2A-Version", "1.0")
            .json(&serde_json::json!({"content": content}));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("A2A steer returned HTTP {status}: {body}"))
        }
    }

    pub(crate) async fn decide_approval(
        &self,
        task_id: &str,
        approval_id: &str,
        decision: crate::ToolApprovalDecision,
    ) -> Result<(), String> {
        if let Some(memory) = &self.memory {
            return memory.decide_approval(task_id, approval_id, decision).await;
        }
        self.post_control(
            task_id,
            "approval",
            serde_json::json!({"approval_id": approval_id, "decision": decision}),
        )
        .await
    }

    pub(crate) async fn answer_question(
        &self,
        task_id: &str,
        question_id: &str,
        response: crate::UserQuestionResponse,
    ) -> Result<(), String> {
        if let Some(memory) = &self.memory {
            return memory.answer_question(task_id, question_id, response).await;
        }
        self.post_control(
            task_id,
            "question",
            serde_json::json!({"question_id": question_id, "response": response}),
        )
        .await
    }

    async fn post_control(&self, task_id: &str, action: &str, body: Value) -> Result<(), String> {
        let mut request = self
            .client
            .post(format!("{}/tasks/{task_id}/remi:{action}", self.endpoint))
            .header("A2A-Version", "1.0")
            .json(&body);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("A2A {action} returned HTTP {status}: {body}"))
        }
    }
}

fn emit_stream_response<F>(
    response: StreamResponse,
    task_id: &mut Option<String>,
    emit: &mut F,
) -> Result<(), String>
where
    F: FnMut(DelegateWireEvent) -> Result<(), String>,
{
    match response {
        StreamResponse::StatusUpdate(update) => {
            if task_id.is_none() {
                *task_id = Some(update.task_id.clone());
                emit(DelegateWireEvent::TaskAssigned {
                    task_id: update.task_id.clone(),
                    context_id: update.context_id.clone(),
                })?;
            }
            if update.status.state.is_terminal() {
                emit(DelegateWireEvent::Terminal {
                    state: update.status.state,
                    message: update.status.message.as_ref().and_then(message_text),
                })?;
            }
        }
        StreamResponse::ArtifactUpdate(update) => {
            if task_id.is_none() {
                *task_id = Some(update.task_id.clone());
                emit(DelegateWireEvent::TaskAssigned {
                    task_id: update.task_id.clone(),
                    context_id: update.context_id.clone(),
                })?;
            }
            if update.artifact.artifact_id == ACTIVITY_ARTIFACT_ID {
                for part in update.artifact.parts {
                    if let PartContent::Data(value) = part.content {
                        emit(DelegateWireEvent::Activity(value))?;
                    }
                }
            }
        }
        StreamResponse::Task(task) => {
            if task_id.is_none() {
                *task_id = Some(task.id.clone());
                emit(DelegateWireEvent::TaskAssigned {
                    task_id: task.id.clone(),
                    context_id: task.context_id.clone(),
                })?;
            }
            if task.status.state.is_terminal() {
                emit(DelegateWireEvent::Terminal {
                    state: task.status.state,
                    message: task.status.message.as_ref().and_then(message_text),
                })?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn message_text(message: &Message) -> Option<String> {
    let text = message
        .parts
        .iter()
        .filter_map(|part| match &part.content {
            PartContent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{
        AgentCapabilities, AgentCard, AgentExtension, AgentInterface, AgentSkill, Artifact,
        TaskArtifactUpdateEvent, TaskStatus, TaskStatusUpdateEvent, TRANSPORT_PROTOCOL_HTTP_JSON,
    };
    use axum::{extract::State, routing::get, routing::post, Json, Router};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct TestState {
        request: Arc<Mutex<Option<SendMessageRequest>>>,
    }

    fn test_stream_events() -> [StreamResponse; 3] {
        let task_id = "server-task".to_string();
        let context_id = "server-context".to_string();
        [
            StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                metadata: None,
            }),
            StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                artifact: Artifact {
                    artifact_id: ACTIVITY_ARTIFACT_ID.to_string(),
                    name: None,
                    description: None,
                    parts: vec![Part::data(serde_json::json!({
                        "event": "tool_call_result",
                        "data": {"call_id": "call-1", "tool_name": "now", "result": "ok"}
                    }))],
                    metadata: None,
                    extensions: Some(vec![ACTIVITY_EXTENSION.to_string()]),
                },
                append: Some(false),
                last_chunk: Some(false),
                metadata: None,
            }),
            StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                context_id,
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                metadata: None,
            }),
        ]
    }

    async fn receive_message(
        State(state): State<TestState>,
        Json(request): Json<SendMessageRequest>,
    ) -> ([(axum::http::HeaderName, &'static str); 1], String) {
        *state.request.lock().unwrap() = Some(request);
        let body = test_stream_events()
            .into_iter()
            .map(|event| format!("data: {}\n\n", serde_json::to_string(&event).unwrap()))
            .collect::<String>();
        (
            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
    }

    #[derive(Clone)]
    struct TestMemoryTransport {
        request: Arc<Mutex<Option<SendMessageRequest>>>,
    }

    #[async_trait::async_trait]
    impl A2aDelegateTransport for TestMemoryTransport {
        async fn invoke(
            &self,
            request: SendMessageRequest,
        ) -> Result<mpsc::Receiver<Result<StreamResponse, String>>, String> {
            *self.request.lock().unwrap() = Some(request);
            let (tx, rx) = mpsc::channel(8);
            for event in test_stream_events() {
                tx.try_send(Ok(event)).unwrap();
            }
            Ok(rx)
        }

        async fn cancel(&self, _task_id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn steer(
            &self,
            _task_id: &str,
            _content: bot_runtime_core::Content,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn decide_approval(
            &self,
            _task_id: &str,
            _approval_id: &str,
            _decision: crate::ToolApprovalDecision,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn answer_question(
            &self,
            _task_id: &str,
            _question_id: &str,
            _response: crate::UserQuestionResponse,
        ) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn invoke_maps_in_memory_a2a_stream_without_http() {
        let request = Arc::new(Mutex::new(None));
        let client = A2aDelegateClient {
            client: reqwest::Client::new(),
            endpoint: "memory://test".to_string(),
            token: None,
            memory: Some(Arc::new(TestMemoryTransport {
                request: Arc::clone(&request),
            })),
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        client
            .invoke(
                "explorer",
                "subagent:explorer:memory",
                "parent-thread",
                "inspect".to_string(),
                CancellationToken::new(),
                move |event| {
                    captured.lock().unwrap().push(event);
                    Ok(())
                },
            )
            .await
            .unwrap();

        let events = events.lock().unwrap();
        assert!(
            matches!(&events[0], DelegateWireEvent::TaskAssigned { task_id, .. } if task_id == "server-task")
        );
        assert!(events.iter().any(|event| matches!(event, DelegateWireEvent::Activity(value) if value["event"] == "tool_call_result")));
        assert!(events.iter().any(|event| matches!(event, DelegateWireEvent::Terminal { state, .. } if *state == TaskState::Completed)));
        assert_eq!(
            request.lock().unwrap().as_ref().unwrap().message.parts[0].as_text(),
            Some("inspect")
        );
    }

    #[tokio::test]
    async fn invoke_discovers_agent_and_maps_real_sse_stream() {
        let request = Arc::new(Mutex::new(None));
        let state = TestState {
            request: Arc::clone(&request),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let card = AgentCard {
            name: "test delegate".to_string(),
            description: "A2A test delegate".to_string(),
            version: "1".to_string(),
            supported_interfaces: vec![AgentInterface::new(
                format!("http://{address}"),
                TRANSPORT_PROTOCOL_HTTP_JSON,
            )],
            capabilities: AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extensions: Some(
                    [ACTIVITY_EXTENSION, INTERACTIVE_EXTENSION, HANDOFF_EXTENSION]
                        .into_iter()
                        .map(|uri| AgentExtension {
                            uri: uri.to_string(),
                            description: None,
                            required: Some(false),
                            params: None,
                        })
                        .collect(),
                ),
                extended_agent_card: Some(false),
            },
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: Vec::<AgentSkill>::new(),
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        };
        let app = Router::new()
            .route(
                "/.well-known/agent-card.json",
                get(move || {
                    let card = card.clone();
                    async move { Json(card) }
                }),
            )
            .route("/message:stream", post(receive_message))
            .with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = A2aDelegateClient {
            client: reqwest::Client::new(),
            endpoint: format!("http://{address}"),
            token: None,
            memory: None,
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        client
            .invoke(
                "explorer",
                "subagent:explorer:e2e",
                "parent-thread",
                "inspect".to_string(),
                CancellationToken::new(),
                move |event| {
                    captured.lock().unwrap().push(event);
                    Ok(())
                },
            )
            .await
            .unwrap();

        let events = events.lock().unwrap();
        assert!(matches!(
            &events[0],
            DelegateWireEvent::TaskAssigned { task_id, context_id }
                if task_id == "server-task" && context_id == "server-context"
        ));
        assert!(events
            .iter()
            .any(|event| matches!(event, DelegateWireEvent::Activity(value) if value["event"] == "tool_call_result")));
        assert!(events.iter().any(
            |event| matches!(event, DelegateWireEvent::Terminal { state, .. } if *state == TaskState::Completed)
        ));
        let request = request.lock().unwrap();
        let message = &request.as_ref().unwrap().message;
        assert_eq!(message.task_id, None);
        assert_eq!(
            message.metadata.as_ref().unwrap()[INVOCATION_EXTENSION]["agentId"],
            "explorer"
        );
        assert_eq!(
            message.metadata.as_ref().unwrap()[INVOCATION_EXTENSION]["parentThreadId"],
            "parent-thread"
        );
        assert_eq!(
            message.extensions.as_ref().unwrap(),
            &vec![
                ACTIVITY_EXTENSION.to_string(),
                INTERACTIVE_EXTENSION.to_string(),
                HANDOFF_EXTENSION.to_string(),
                INVOCATION_EXTENSION.to_string(),
            ]
        );
        server.abort();
    }
}
