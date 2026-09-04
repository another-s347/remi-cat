use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a2a::{
    A2AError, AgentCapabilities, AgentCard, AgentExtension, AgentInterface, AgentSkill, Artifact,
    HttpAuthSecurityScheme, ListTasksRequest, ListTasksResponse, Message, Part, PartContent, Role,
    SecurityScheme, SendMessageRequest, StreamResponse, Task, TaskArtifactUpdateEvent, TaskState,
    TaskStatus, TaskStatusUpdateEvent, TRANSPORT_PROTOCOL_HTTP_JSON,
};
use a2a_server::{AgentExecutor, DefaultRequestHandler, ExecutorContext, TaskStore};
use async_stream::try_stream;
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use crate::application::{
    ApplicationEvent, ApplicationHandle, ChannelConfig, ChannelHandle, RunControl, RunOptions,
    RunRequest,
};
use crate::session::SessionRuntime;
use crate::web_chat::{WebChatHandle, WebRuntimeOptions};

const A2A_CHANNEL: &str = "a2a";
const DEFAULT_A2A_HOST: &str = "127.0.0.1";
const DEFAULT_A2A_PORT: &str = "8788";
const ANSWER_ARTIFACT_ID: &str = "answer";
const ACTIVITY_ARTIFACT_ID: &str = "remi-activity";
const ACTIVITY_EXTENSION: &str = "urn:remi:a2a:activity-stream:v1";
const INTERACTIVE_EXTENSION: &str = "urn:remi:a2a:interactive-input:v1";
const HANDOFF_EXTENSION: &str = "urn:remi:a2a:handoff:v1";
const INVOCATION_EXTENSION: &str = "urn:remi:a2a:invocation-context:v1";

#[derive(Clone)]
struct RemiA2aExecutor {
    web_chat: WebChatHandle,
    sessions: Arc<Mutex<SessionRuntime>>,
    root_agent_id: String,
    active_sessions: Arc<RwLock<HashMap<String, String>>>,
}

#[derive(Clone)]
pub(crate) struct ApplicationA2aExecutor {
    channel: ChannelHandle,
    active: Arc<RwLock<HashMap<String, (String, RunControl)>>>,
}

#[derive(Clone)]
pub(crate) struct ApplicationMemoryA2aTransport {
    executor: ApplicationA2aExecutor,
}

impl ApplicationMemoryA2aTransport {
    pub(crate) fn new(application: ApplicationHandle) -> Self {
        Self {
            executor: ApplicationA2aExecutor::new(application),
        }
    }

    fn control_context(task_id: &str) -> ExecutorContext {
        ExecutorContext {
            message: None,
            task_id: task_id.to_string(),
            stored_task: None,
            context_id: String::new(),
            metadata: None,
            user: None,
            service_params: HashMap::new(),
            tenant: None,
        }
    }
}

#[async_trait]
impl bot_core::A2aDelegateTransport for ApplicationMemoryA2aTransport {
    async fn invoke(
        &self,
        request: SendMessageRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamResponse, String>>, String> {
        let message = request.message;
        let context_id = message
            .context_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let task_id = message
            .task_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let context = ExecutorContext {
            message: Some(message),
            task_id,
            stored_task: None,
            context_id,
            metadata: request.metadata,
            user: None,
            service_params: HashMap::new(),
            tenant: request.tenant,
        };
        let mut stream = self.executor.execute(context);
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(event) = stream.next().await {
                if tx
                    .send(event.map_err(|error| error.to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Ok(rx)
    }

    async fn cancel(&self, task_id: &str) -> Result<(), String> {
        use futures::StreamExt;
        let mut stream = self.executor.cancel(Self::control_context(task_id));
        match stream.next().await {
            Some(Ok(_)) => Ok(()),
            Some(Err(error)) => Err(error.to_string()),
            None => Err("in-memory A2A cancel returned no response".to_string()),
        }
    }

    async fn steer(&self, task_id: &str, content: bot_core::Content) -> Result<(), String> {
        let session_id = self
            .executor
            .active
            .read()
            .await
            .get(task_id)
            .map(|(id, _)| id.clone())
            .ok_or_else(|| "A2A task is not active".to_string())?;
        self.executor
            .channel
            .steer(RunRequest::new(session_id, content))
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    async fn decide_approval(
        &self,
        task_id: &str,
        approval_id: &str,
        decision: bot_core::ToolApprovalDecision,
    ) -> Result<(), String> {
        if !self.executor.active.read().await.contains_key(task_id) {
            return Err("A2A task is not active".to_string());
        }
        self.executor
            .channel
            .decide_approval(approval_id, decision)
            .await
            .map_err(|error| error.to_string())?
            .map(|_| ())
            .ok_or_else(|| "approval is not pending".to_string())
    }

    async fn answer_question(
        &self,
        task_id: &str,
        question_id: &str,
        response: bot_core::UserQuestionResponse,
    ) -> Result<(), String> {
        if !self.executor.active.read().await.contains_key(task_id) {
            return Err("A2A task is not active".to_string());
        }
        self.executor
            .channel
            .answer_user_question(question_id, response)
            .await
            .map_err(|error| error.to_string())?
            .map(|_| ())
            .ok_or_else(|| "question is not pending".to_string())
    }
}

impl ApplicationA2aExecutor {
    pub(crate) fn new(application: ApplicationHandle) -> Self {
        Self {
            channel: application.channel(ChannelConfig::new(A2A_CHANNEL)),
            active: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl AgentExecutor for ApplicationA2aExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let channel = self.channel.clone();
        let active = Arc::clone(&self.active);
        Box::pin(try_stream! {
            let message = ctx.message.as_ref().ok_or_else(|| A2AError::invalid_request("message is required"))?;
            let text = message_text(message)?;
            let input_message = message.clone();
            let agent_id = target_agent_id(message);
            let output_context = a2a_output_context(message);
            let session = channel
                .resolve_session(ctx.context_id.clone())
                .await
                .map_err(|error| A2AError::internal(error.to_string()))?;
            let options = agent_id.map_or_else(RunOptions::default, |agent| RunOptions::default().agent(agent));
            let mut run = channel
                .run(
                    RunRequest::text(session.id.clone(), text)
                        .options(options)
                        .with_optional_output_context(output_context),
                )
                .await
                .map_err(|error| A2AError::internal(error.to_string()))?;
            active.write().await.insert(ctx.task_id.clone(), (session.id, run.control()));
            yield status_event(&ctx, TaskState::Working, None);

            let mut final_output = String::new();
            let mut terminal = TaskState::Completed;
            let mut terminal_message = None;
            while let Some(event) = run.recv().await {
                match event {
                    ApplicationEvent::Prefix(text) | ApplicationEvent::Reply(text) => {
                        final_output.push_str(&text);
                        yield activity_event(&ctx, activity_value("text_delta", serde_json::json!({"text": text})));
                    }
                    ApplicationEvent::Cat(event) => {
                        if let Some((kind, data)) = application_cat_activity(event) {
                            if kind == "text_delta" {
                                if let Some(text) = data.get("text").and_then(serde_json::Value::as_str) {
                                    final_output.push_str(text);
                                }
                            }
                            if kind == "error" {
                                terminal = TaskState::Failed;
                                terminal_message = data.get("message").and_then(serde_json::Value::as_str).map(str::to_string);
                            }
                            yield activity_event(&ctx, activity_value(kind, data));
                        }
                    }
                    ApplicationEvent::ResponseCompleted { text, .. } => {
                        final_output = text;
                    }
                    ApplicationEvent::Done => break,
                    ApplicationEvent::SupervisorStarted => {}
                }
            }
            active.write().await.remove(&ctx.task_id);
            if terminal == TaskState::Completed {
                yield completed_task(&ctx, input_message, final_output);
            } else {
                yield status_event(&ctx, terminal, terminal_message);
            }
        })
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let active = Arc::clone(&self.active);
        Box::pin(try_stream! {
            let control = active.read().await.get(&ctx.task_id).map(|(_, control)| control.clone());
            let Some(control) = control else { Err(A2AError::task_not_cancelable(&ctx.task_id))?; unreachable!() };
            if !control.cancel().await.map_err(|error| A2AError::internal(error.to_string()))? {
                Err(A2AError::task_not_cancelable(&ctx.task_id))?;
            }
            yield status_event(&ctx, TaskState::Canceled, None);
        })
    }
}

fn target_agent_id(message: &Message) -> Option<String> {
    message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(INVOCATION_EXTENSION))
        .and_then(|value| value.get("agentId"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn a2a_output_context(message: &Message) -> Option<crate::OutputProtocolContext> {
    message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(INVOCATION_EXTENSION))
        .and_then(|value| value.get("outputContext"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn activity_value(kind: &str, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"version": 1, "event": kind, "data": data})
}

fn application_cat_activity(
    event: bot_core::CatEvent,
) -> Option<(&'static str, serde_json::Value)> {
    match event {
        bot_core::CatEvent::Text(text) => Some(("text_delta", serde_json::json!({"text": text}))),
        bot_core::CatEvent::Thinking(text) => {
            Some(("thinking_delta", serde_json::json!({"text": text})))
        }
        bot_core::CatEvent::ToolCallStart { id, name }
        | bot_core::CatEvent::ToolCall { id, name, .. } => Some((
            "tool_call_started",
            serde_json::json!({"call_id": id, "tool_name": name}),
        )),
        bot_core::CatEvent::ToolCallArgumentsDelta { id, delta } => Some((
            "tool_call_arguments_delta",
            serde_json::json!({"call_id": id, "delta": delta}),
        )),
        bot_core::CatEvent::ToolCallResult {
            id, name, result, ..
        } => Some((
            "tool_call_result",
            serde_json::json!({"call_id": id, "tool_name": name, "result": result}),
        )),
        bot_core::CatEvent::ToolApprovalRequested(request) => {
            Some(("approval_requested", serde_json::to_value(request).ok()?))
        }
        bot_core::CatEvent::ToolApprovalUpdated(request) => {
            Some(("approval_updated", serde_json::to_value(request).ok()?))
        }
        bot_core::CatEvent::ToolApprovalResolved { request, decision } => Some((
            "approval_resolved",
            serde_json::json!({"request": request, "decision": decision}),
        )),
        bot_core::CatEvent::UserQuestionRequested(request) => Some((
            "user_question_requested",
            serde_json::to_value(request).ok()?,
        )),
        bot_core::CatEvent::UserQuestionUpdated(request) => {
            Some(("user_question_updated", serde_json::to_value(request).ok()?))
        }
        bot_core::CatEvent::UserQuestionResolved { request, response } => Some((
            "user_question_resolved",
            serde_json::json!({"request": request, "response": response}),
        )),
        bot_core::CatEvent::SubSession(event) => {
            Some(("sub_session", serde_json::to_value(event).ok()?))
        }
        bot_core::CatEvent::SteerInjected(event) => {
            Some(("steer_injected", serde_json::to_value(event).ok()?))
        }
        bot_core::CatEvent::Error(error) => {
            Some(("error", serde_json::json!({"message": error.to_string()})))
        }
        _ => None,
    }
}

impl RemiA2aExecutor {
    fn new(
        web_chat: WebChatHandle,
        sessions: Arc<Mutex<SessionRuntime>>,
        root_agent_id: String,
        active_sessions: Arc<RwLock<HashMap<String, String>>>,
    ) -> Self {
        Self {
            web_chat,
            sessions,
            root_agent_id,
            active_sessions,
        }
    }
}

impl AgentExecutor for RemiA2aExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let web_chat = self.web_chat.clone();
        let sessions = Arc::clone(&self.sessions);
        let root_agent_id = self.root_agent_id.clone();
        let active_sessions = Arc::clone(&self.active_sessions);

        Box::pin(try_stream! {
            let message = ctx
                .message
                .as_ref()
                .ok_or_else(|| A2AError::invalid_request("message is required"))?;
            let text = message_text(&message)?;
            let input_message = message.clone();
            let target_agent_id = message
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get(INVOCATION_EXTENSION))
                .and_then(|value| value.get("agentId"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let output_context = a2a_output_context(message);
            let session_id = sessions
                .lock()
                .await
                .resolve_channel(A2A_CHANNEL, &ctx.context_id, &root_agent_id)
                .map_err(|error| A2AError::internal(error.to_string()))?;
            active_sessions
                .write()
                .await
                .insert(ctx.task_id.clone(), session_id.clone());

            yield status_event(&ctx, TaskState::Working, None);

            let mut run = web_chat
                .run(
                    session_id,
                    ctx.task_id.clone(),
                    bot_core::Content::text(text),
                    WebRuntimeOptions {
                        agent_id: target_agent_id,
                        platform: Some(A2A_CHANNEL.to_string()),
                        output_context,
                        ..WebRuntimeOptions::default()
                    },
                    None,
                )
                .await
                .map_err(|error| A2AError::internal(error.to_string()))?;

            // Keep one delta pending so the final non-empty artifact chunk can
            // carry `lastChunk=true` without emitting a synthetic empty part.
            let mut pending_text: Option<String> = None;
            let mut complete_text = String::new();
            let mut append = false;
            let mut terminal_state = None;
            let mut terminal_message = None;

            while let Some(event) = run.events.recv().await {
                yield activity_event(&ctx, serde_json::to_value(&event)
                    .map_err(|error| A2AError::internal(error.to_string()))?);
                match event.event.as_str() {
                    "text_delta" => {
                        let Some(text) = event
                            .data
                            .as_ref()
                            .and_then(|data| data.get("text"))
                            .and_then(serde_json::Value::as_str)
                            .filter(|text| !text.is_empty())
                        else {
                            continue;
                        };
                        complete_text.push_str(text);
                        if let Some(previous) = pending_text.replace(text.to_string()) {
                            yield artifact_event(&ctx, previous, append, false);
                            append = true;
                        }
                    }
                    "cancelled" => terminal_state = Some(TaskState::Canceled),
                    "error" => {
                        terminal_state = Some(TaskState::Failed);
                        terminal_message = event
                            .data
                            .as_ref()
                            .and_then(|data| data.get("message"))
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string);
                    }
                    "run_finished" => {
                        let status = event
                            .data
                            .as_ref()
                            .and_then(|data| data.get("status"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("completed");
                        terminal_state = Some(match status {
                            "cancelled" | "interrupted" => TaskState::Canceled,
                            "error" => TaskState::Failed,
                            _ => TaskState::Completed,
                        });
                        break;
                    }
                    _ => {}
                }
            }

            if let Some(text) = pending_text {
                yield artifact_event(&ctx, text, append, true);
            }
            let state = terminal_state.unwrap_or(TaskState::Failed);
            if terminal_message.is_none() && state == TaskState::Failed {
                terminal_message = Some("remi-cat run ended without a terminal event".to_string());
            }
            if state == TaskState::Completed {
                yield completed_task(&ctx, input_message, complete_text);
            } else {
                yield status_event(&ctx, state, terminal_message);
            }
            active_sessions.write().await.remove(&ctx.task_id);
        })
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let web_chat = self.web_chat.clone();
        Box::pin(try_stream! {
            if !web_chat
                .cancel(ctx.task_id.clone())
                .await
                .map_err(|error| A2AError::internal(error.to_string()))?
            {
                Err(A2AError::task_not_cancelable(&ctx.task_id))?;
            }
            yield status_event(&ctx, TaskState::Canceled, None);
        })
    }
}

#[derive(Clone)]
struct ControlState {
    web_chat: WebChatHandle,
    active_sessions: Arc<RwLock<HashMap<String, String>>>,
}

#[derive(Deserialize)]
struct SteerRequest {
    content: bot_core::Content,
}

#[derive(Deserialize)]
struct ApprovalDecisionRequest {
    approval_id: String,
    decision: bot_core::ToolApprovalDecision,
}

#[derive(Deserialize)]
struct QuestionAnswerRequest {
    question_id: String,
    response: bot_core::UserQuestionResponse,
}

async fn steer_task(
    State(state): State<ControlState>,
    AxumPath(task_id): AxumPath<String>,
    axum::Json(request): axum::Json<SteerRequest>,
) -> Response {
    let session_id = state.active_sessions.read().await.get(&task_id).cloned();
    let Some(session_id) = session_id else {
        return (StatusCode::NOT_FOUND, "A2A task is not active").into_response();
    };
    match state
        .web_chat
        .steer(session_id, task_id, request.content)
        .await
    {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
    }
}

async fn decide_task_approval(
    State(state): State<ControlState>,
    AxumPath(task_id): AxumPath<String>,
    axum::Json(request): axum::Json<ApprovalDecisionRequest>,
) -> Response {
    if !state.active_sessions.read().await.contains_key(&task_id) {
        return (StatusCode::NOT_FOUND, "A2A task is not active").into_response();
    }
    match state
        .web_chat
        .decide_approval(request.approval_id, request.decision)
        .await
    {
        Ok(Some(_)) => StatusCode::ACCEPTED.into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "approval is not pending").into_response(),
        Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
    }
}

async fn answer_task_question(
    State(state): State<ControlState>,
    AxumPath(task_id): AxumPath<String>,
    axum::Json(request): axum::Json<QuestionAnswerRequest>,
) -> Response {
    if !state.active_sessions.read().await.contains_key(&task_id) {
        return (StatusCode::NOT_FOUND, "A2A task is not active").into_response();
    }
    match state
        .web_chat
        .answer_user_question(request.question_id, request.response)
        .await
    {
        Ok(Some(_)) => StatusCode::ACCEPTED.into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "question is not pending").into_response(),
        Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
    }
}

#[derive(Clone)]
struct ApplicationControlState {
    channel: ChannelHandle,
    active: Arc<RwLock<HashMap<String, (String, RunControl)>>>,
}

async fn steer_application_task(
    State(state): State<ApplicationControlState>,
    AxumPath(task_id): AxumPath<String>,
    axum::Json(request): axum::Json<SteerRequest>,
) -> Response {
    let session_id = state
        .active
        .read()
        .await
        .get(&task_id)
        .map(|(id, _)| id.clone());
    let Some(session_id) = session_id else {
        return (StatusCode::NOT_FOUND, "A2A task is not active").into_response();
    };
    match state
        .channel
        .steer(RunRequest::new(session_id, request.content))
        .await
    {
        Ok(_) => StatusCode::ACCEPTED.into_response(),
        Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
    }
}

async fn decide_application_task_approval(
    State(state): State<ApplicationControlState>,
    AxumPath(task_id): AxumPath<String>,
    axum::Json(request): axum::Json<ApprovalDecisionRequest>,
) -> Response {
    if !state.active.read().await.contains_key(&task_id) {
        return (StatusCode::NOT_FOUND, "A2A task is not active").into_response();
    }
    match state
        .channel
        .decide_approval(request.approval_id, request.decision)
        .await
    {
        Ok(Some(_)) => StatusCode::ACCEPTED.into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "approval is not pending").into_response(),
        Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
    }
}

async fn answer_application_task_question(
    State(state): State<ApplicationControlState>,
    AxumPath(task_id): AxumPath<String>,
    axum::Json(request): axum::Json<QuestionAnswerRequest>,
) -> Response {
    if !state.active.read().await.contains_key(&task_id) {
        return (StatusCode::NOT_FOUND, "A2A task is not active").into_response();
    }
    match state
        .channel
        .answer_user_question(request.question_id, request.response)
        .await
    {
        Ok(Some(_)) => StatusCode::ACCEPTED.into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "question is not pending").into_response(),
        Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
    }
}

fn message_text(message: &Message) -> Result<String, A2AError> {
    if message.role != Role::User {
        return Err(A2AError::invalid_request("message role must be ROLE_USER"));
    }
    let mut text = Vec::new();
    for part in &message.parts {
        match &part.content {
            PartContent::Text(value) => text.push(value.as_str()),
            _ => return Err(A2AError::content_type_not_supported()),
        }
    }
    let text = text.join("\n");
    if text.trim().is_empty() {
        return Err(A2AError::invalid_request("message text may not be empty"));
    }
    Ok(text)
}

fn status_event(
    ctx: &ExecutorContext,
    state: TaskState,
    message: Option<String>,
) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        status: TaskStatus {
            state,
            message: message.map(|text| Message::new(Role::Agent, vec![Part::text(text)])),
            timestamp: Some(chrono::Utc::now()),
        },
        metadata: None,
    })
}

fn artifact_event(
    ctx: &ExecutorContext,
    text: String,
    append: bool,
    last_chunk: bool,
) -> StreamResponse {
    StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        artifact: Artifact {
            artifact_id: ANSWER_ARTIFACT_ID.to_string(),
            name: Some("answer".to_string()),
            description: Some("remi-cat final answer".to_string()),
            parts: vec![Part::text(text)],
            metadata: None,
            extensions: None,
        },
        append: Some(append),
        last_chunk: Some(last_chunk),
        metadata: None,
    })
}

fn activity_event(ctx: &ExecutorContext, event: serde_json::Value) -> StreamResponse {
    StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        artifact: Artifact {
            artifact_id: ACTIVITY_ARTIFACT_ID.to_string(),
            name: Some("remi activity".to_string()),
            description: Some("Structured remi-cat runtime activity".to_string()),
            parts: vec![Part::data(event)],
            metadata: None,
            extensions: Some(vec![ACTIVITY_EXTENSION.to_string()]),
        },
        append: Some(false),
        last_chunk: Some(false),
        metadata: None,
    })
}

fn remi_extensions() -> Vec<AgentExtension> {
    [
        (ACTIVITY_EXTENSION, "Structured runtime activity stream"),
        (
            INTERACTIVE_EXTENSION,
            "Approval and user-question continuation",
        ),
        (HANDOFF_EXTENSION, "Steer a working named delegate task"),
        (
            INVOCATION_EXTENSION,
            "Target agent and parent invocation context",
        ),
    ]
    .into_iter()
    .map(|(uri, description)| AgentExtension {
        uri: uri.to_string(),
        description: Some(description.to_string()),
        required: Some(false),
        params: None,
    })
    .collect()
}

fn completed_task(ctx: &ExecutorContext, input: Message, text: String) -> StreamResponse {
    let mut history = ctx
        .stored_task
        .as_ref()
        .and_then(|task| task.history.clone())
        .unwrap_or_default();
    if !history
        .iter()
        .any(|message| message.message_id == input.message_id)
    {
        history.push(input);
    }
    let artifacts = (!text.is_empty()).then(|| {
        vec![Artifact {
            artifact_id: ANSWER_ARTIFACT_ID.to_string(),
            name: Some("answer".to_string()),
            description: Some("remi-cat final answer".to_string()),
            parts: vec![Part::text(text)],
            metadata: None,
            extensions: None,
        }]
    });
    StreamResponse::Task(Task {
        id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        status: TaskStatus {
            state: TaskState::Completed,
            message: None,
            timestamp: Some(chrono::Utc::now()),
        },
        artifacts,
        history: Some(history),
        metadata: ctx
            .stored_task
            .as_ref()
            .and_then(|task| task.metadata.clone()),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedTaskEntry {
    task: Task,
    version: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedTaskFile {
    tasks: HashMap<String, PersistedTaskEntry>,
}

struct JsonTaskStore {
    path: PathBuf,
    tasks: RwLock<HashMap<String, PersistedTaskEntry>>,
}

impl JsonTaskStore {
    fn load(path: PathBuf) -> anyhow::Result<Self> {
        let tasks = match std::fs::read_to_string(&path) {
            Ok(raw) if raw.trim().is_empty() => HashMap::new(),
            Ok(raw) => serde_json::from_str::<PersistedTaskFile>(&raw)?.tasks,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            path,
            tasks: RwLock::new(tasks),
        })
    }

    async fn save(&self, tasks: &HashMap<String, PersistedTaskEntry>) -> Result<(), A2AError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| A2AError::internal(error.to_string()))?;
        let data = serde_json::to_vec_pretty(&PersistedTaskFile {
            tasks: tasks.clone(),
        })
        .map_err(|error| A2AError::internal(error.to_string()))?;
        let temp = self.path.with_extension("json.tmp");
        tokio::fs::write(&temp, data)
            .await
            .map_err(|error| A2AError::internal(error.to_string()))?;
        tokio::fs::rename(&temp, &self.path)
            .await
            .map_err(|error| A2AError::internal(error.to_string()))
    }
}

#[async_trait]
impl TaskStore for JsonTaskStore {
    async fn create(&self, task: Task) -> Result<u64, A2AError> {
        let mut tasks = self.tasks.write().await;
        if tasks.contains_key(&task.id) {
            return Err(A2AError::internal("task already exists"));
        }
        tasks.insert(task.id.clone(), PersistedTaskEntry { task, version: 1 });
        self.save(&tasks).await?;
        Ok(1)
    }

    async fn update(&self, mut task: Task) -> Result<u64, A2AError> {
        let mut tasks = self.tasks.write().await;
        let entry = tasks
            .get_mut(&task.id)
            .ok_or_else(|| A2AError::task_not_found(&task.id))?;
        // Executor activity and CancelTask are driven by separate async
        // streams. An activity update may already hold a stale WORKING
        // snapshot when cancellation commits. Never let that late snapshot,
        // or any other delayed event, regress an acknowledged terminal state.
        if entry.task.status.state.is_terminal() {
            task.status = entry.task.status.clone();
        }
        entry.version += 1;
        entry.task = task;
        let version = entry.version;
        self.save(&tasks).await?;
        Ok(version)
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        Ok(self
            .tasks
            .read()
            .await
            .get(task_id)
            .map(|entry| entry.task.clone()))
    }

    async fn list(&self, request: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        let tasks = self.tasks.read().await;
        let mut matches = tasks
            .values()
            .filter(|entry| {
                request
                    .context_id
                    .as_ref()
                    .is_none_or(|context_id| entry.task.context_id == *context_id)
                    && request
                        .status
                        .as_ref()
                        .is_none_or(|status| entry.task.status.state == *status)
            })
            .map(|entry| entry.task.clone())
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.id.cmp(&right.id));

        let page_size = request.page_size.filter(|size| *size > 0).unwrap_or(50) as usize;
        let start = request
            .page_token
            .as_deref()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
            .min(matches.len());
        let end = (start + page_size).min(matches.len());
        let total_size = matches.len();
        let mut page = matches[start..end].to_vec();
        if let Some(history_length) = request.history_length {
            let keep = history_length.max(0) as usize;
            for task in &mut page {
                if let Some(history) = &mut task.history {
                    let remove = history.len().saturating_sub(keep);
                    history.drain(..remove);
                }
            }
        }
        Ok(ListTasksResponse {
            tasks: page,
            next_page_token: (end < total_size)
                .then(|| end.to_string())
                .unwrap_or_default(),
            page_size: page_size as i32,
            total_size: total_size as i32,
        })
    }
}

#[derive(Clone)]
struct AuthState {
    token_digest: Option<[u8; 32]>,
}

impl AuthState {
    fn new(token: Option<&str>) -> Self {
        Self {
            token_digest: token.map(token_digest),
        }
    }
}

fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn digest_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

async fn require_bearer(
    State(auth): State<AuthState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Response {
    let Some(expected) = auth.token_digest else {
        return next.run(request).await;
    };
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(token_digest);
    if supplied
        .as_ref()
        .is_some_and(|value| digest_eq(&expected, value))
    {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "missing or invalid bearer token",
        )
            .into_response()
    }
}

fn agent_card(public_url: &str, token_required: bool) -> AgentCard {
    let (security_schemes, security_requirements) = if token_required {
        let schemes = HashMap::from([(
            "bearer".to_string(),
            SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
                scheme: "bearer".to_string(),
                description: Some("remi-cat A2A bearer token".to_string()),
                bearer_format: None,
            }),
        )]);
        let requirements = vec![HashMap::from([("bearer".to_string(), Vec::new())])];
        (Some(schemes), Some(requirements))
    } else {
        (None, None)
    };
    AgentCard {
        name: "remi-cat".to_string(),
        description: "Multi-agent AI assistant exposed through A2A".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        supported_interfaces: vec![AgentInterface::new(
            public_url.trim_end_matches('/'),
            TRANSPORT_PROTOCOL_HTTP_JSON,
        )],
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extensions: Some(remi_extensions()),
            extended_agent_card: Some(false),
        },
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        skills: vec![AgentSkill {
            id: "general-assistant".to_string(),
            name: "General assistant".to_string(),
            description: "Use remi-cat agents and tools to complete a task".to_string(),
            tags: vec!["assistant".to_string(), "tools".to_string()],
            examples: None,
            input_modes: Some(vec!["text/plain".to_string()]),
            output_modes: Some(vec!["text/plain".to_string()]),
            security_requirements: None,
        }],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes,
        security_requirements,
        signatures: None,
    }
}

pub(crate) fn stdio_agent_card(profile: &crate::application::ApplicationProfileInfo) -> AgentCard {
    let skills = if profile.capabilities.intents.is_empty() {
        vec![AgentSkill {
            id: profile.id.clone(),
            name: profile.name.clone(),
            description: profile
                .description
                .clone()
                .unwrap_or_else(|| format!("{} external agent", profile.name)),
            tags: profile.capabilities.tags.clone(),
            examples: None,
            input_modes: Some(vec!["text/plain".to_string()]),
            output_modes: Some(vec!["text/plain".to_string()]),
            security_requirements: None,
        }]
    } else {
        profile
            .capabilities
            .intents
            .iter()
            .map(|intent| AgentSkill {
                id: intent.clone(),
                name: intent.clone(),
                description: profile
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("{} capability", profile.name)),
                tags: profile.capabilities.tags.clone(),
                examples: None,
                input_modes: Some(vec!["text/plain".to_string()]),
                output_modes: Some(vec!["text/plain".to_string()]),
                security_requirements: None,
            })
            .collect()
    };
    AgentCard {
        name: profile.name.clone(),
        description: profile
            .description
            .clone()
            .unwrap_or_else(|| format!("{} external agent", profile.name)),
        version: profile.version.clone().unwrap_or_else(|| "1".to_string()),
        supported_interfaces: vec![AgentInterface::new(
            "stdio://local",
            "urn:remi:a2a:binding:stdio-json:v1",
        )],
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extensions: Some(remi_extensions()),
            extended_agent_card: Some(false),
        },
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        skills,
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: None,
        security_requirements: None,
        signatures: None,
    }
}

fn router(
    web_chat: WebChatHandle,
    sessions: Arc<Mutex<SessionRuntime>>,
    root_agent_id: String,
    task_store: JsonTaskStore,
    public_url: &str,
    token: Option<&str>,
) -> Router {
    let active_sessions = Arc::new(RwLock::new(HashMap::new()));
    let capabilities = AgentCapabilities {
        streaming: Some(true),
        push_notifications: Some(false),
        extensions: Some(remi_extensions()),
        extended_agent_card: Some(false),
    };
    let handler = Arc::new(
        DefaultRequestHandler::new(
            RemiA2aExecutor::new(
                web_chat.clone(),
                sessions,
                root_agent_id,
                Arc::clone(&active_sessions),
            ),
            task_store,
        )
        .with_capabilities(capabilities),
    );
    let auth = AuthState::new(token);
    let protected = a2a_server::rest::rest_router(handler)
        .layer(middleware::from_fn_with_state(auth.clone(), require_bearer));
    let control = Router::new()
        .route(
            "/tasks/{task_id}/remi:steer",
            axum::routing::post(steer_task),
        )
        .route(
            "/tasks/{task_id}/remi:approval",
            axum::routing::post(decide_task_approval),
        )
        .route(
            "/tasks/{task_id}/remi:question",
            axum::routing::post(answer_task_question),
        )
        .with_state(ControlState {
            web_chat,
            active_sessions,
        })
        .layer(middleware::from_fn_with_state(auth, require_bearer));
    let card = Arc::new(a2a_server::StaticAgentCard::new(agent_card(
        public_url,
        token.is_some(),
    )));
    a2a_server::agent_card::agent_card_router(card)
        .merge(protected)
        .merge(control)
}

fn application_router(
    application: ApplicationHandle,
    task_store: JsonTaskStore,
    public_url: &str,
    token: Option<&str>,
) -> Router {
    let card_profile = application.profile().clone();
    let channel = application.channel(ChannelConfig::new(A2A_CHANNEL));
    let active = Arc::new(RwLock::new(HashMap::new()));
    let capabilities = AgentCapabilities {
        streaming: Some(true),
        push_notifications: Some(false),
        extensions: Some(remi_extensions()),
        extended_agent_card: Some(false),
    };
    let handler = Arc::new(
        DefaultRequestHandler::new(
            ApplicationA2aExecutor {
                channel: channel.clone(),
                active: Arc::clone(&active),
            },
            task_store,
        )
        .with_capabilities(capabilities),
    );
    let auth = AuthState::new(token);
    let protected = a2a_server::rest::rest_router(handler)
        .layer(middleware::from_fn_with_state(auth.clone(), require_bearer));
    let control = Router::new()
        .route(
            "/tasks/{task_id}/remi:steer",
            axum::routing::post(steer_application_task),
        )
        .route(
            "/tasks/{task_id}/remi:approval",
            axum::routing::post(decide_application_task_approval),
        )
        .route(
            "/tasks/{task_id}/remi:question",
            axum::routing::post(answer_application_task_question),
        )
        .with_state(ApplicationControlState { channel, active })
        .layer(middleware::from_fn_with_state(auth, require_bearer));
    let mut profile_card = stdio_agent_card(&card_profile);
    profile_card.supported_interfaces = vec![AgentInterface::new(
        public_url.trim_end_matches('/'),
        TRANSPORT_PROTOCOL_HTTP_JSON,
    )];
    if token.is_some() {
        profile_card.security_schemes = Some(HashMap::from([(
            "bearer".to_string(),
            SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
                scheme: "bearer".to_string(),
                description: Some("remi-cat A2A bearer token".to_string()),
                bearer_format: None,
            }),
        )]));
        profile_card.security_requirements =
            Some(vec![HashMap::from([("bearer".to_string(), Vec::new())])]);
    }
    let card = Arc::new(a2a_server::StaticAgentCard::new(profile_card));
    a2a_server::agent_card::agent_card_router(card)
        .merge(protected)
        .merge(control)
}

pub(crate) async fn maybe_start(
    web_chat: WebChatHandle,
    sessions: Arc<Mutex<SessionRuntime>>,
    root_agent_id: String,
    data_dir: PathBuf,
    required_for_delegates: bool,
) -> anyhow::Result<()> {
    let enabled = std::env::var("REMI_A2A_ENABLED")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True" | "yes" | "on"));
    if !enabled && !required_for_delegates {
        return Ok(());
    }

    let host = std::env::var("REMI_A2A_HOST").unwrap_or_else(|_| DEFAULT_A2A_HOST.to_string());
    let port = std::env::var("REMI_A2A_PORT").unwrap_or_else(|_| DEFAULT_A2A_PORT.to_string());
    let token = std::env::var("REMI_A2A_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if !is_loopback_host(&host) && token.is_none() {
        anyhow::bail!("REMI_A2A_TOKEN is required when REMI_A2A_HOST is not loopback");
    }

    let address = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&address).await?;
    let local_address = listener.local_addr()?;
    let public_url =
        std::env::var("REMI_A2A_PUBLIC_URL").unwrap_or_else(|_| format!("http://{local_address}"));
    let task_store = JsonTaskStore::load(data_dir.join("a2a").join("tasks.json"))?;
    let app = router(
        web_chat,
        sessions,
        root_agent_id,
        task_store,
        &public_url,
        token.as_deref(),
    );
    tokio::spawn(async move {
        info!(address = %local_address, "remi-cat A2A server listening");
        if let Err(error) = axum::serve(listener, app).await {
            warn!(error = %error, "A2A server stopped");
        }
    });
    Ok(())
}

pub(crate) async fn maybe_start_application(
    application: ApplicationHandle,
    data_dir: PathBuf,
) -> anyhow::Result<()> {
    let enabled = std::env::var("REMI_A2A_ENABLED")
        .ok()
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True" | "yes" | "on"));
    if !enabled {
        return Ok(());
    }
    let host = std::env::var("REMI_A2A_HOST").unwrap_or_else(|_| DEFAULT_A2A_HOST.to_string());
    let port = std::env::var("REMI_A2A_PORT").unwrap_or_else(|_| DEFAULT_A2A_PORT.to_string());
    let token = std::env::var("REMI_A2A_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if !is_loopback_host(&host) && token.is_none() {
        anyhow::bail!("REMI_A2A_TOKEN is required when REMI_A2A_HOST is not loopback");
    }
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await?;
    let local_address = listener.local_addr()?;
    let public_url =
        std::env::var("REMI_A2A_PUBLIC_URL").unwrap_or_else(|_| format!("http://{local_address}"));
    let app = application_router(
        application,
        JsonTaskStore::load(data_dir.join("a2a").join("tasks.json"))?,
        &public_url,
        token.as_deref(),
    );
    tokio::spawn(async move {
        info!(address = %local_address, "remi-cat Application A2A server listening");
        if let Err(error) = axum::serve(listener, app).await {
            warn!(error = %error, "A2A server stopped");
        }
    });
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host.trim(), "127.0.0.1" | "::1" | "localhost")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_chat::{ChatEventV1, WebChatCommand, WebRun};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn text_message_is_joined_and_non_text_is_rejected() {
        let message = Message::new(Role::User, vec![Part::text("one"), Part::text("two")]);
        assert_eq!(message_text(&message).unwrap(), "one\ntwo");

        let message = Message::new(Role::User, vec![Part::data(serde_json::json!({"x": 1}))]);
        assert_eq!(
            message_text(&message).unwrap_err().code,
            a2a::error_code::CONTENT_TYPE_NOT_SUPPORTED
        );
    }

    #[test]
    fn agent_card_matches_mvp_capabilities() {
        let card = agent_card("http://127.0.0.1:8788", true);
        assert_eq!(card.supported_interfaces.len(), 1);
        assert_eq!(
            card.supported_interfaces[0].protocol_binding,
            TRANSPORT_PROTOCOL_HTTP_JSON
        );
        assert_eq!(card.capabilities.streaming, Some(true));
        assert_eq!(card.capabilities.push_notifications, Some(false));
        assert_eq!(card.capabilities.extensions.as_ref().unwrap().len(), 4);
        assert!(card.security_schemes.is_some());
    }

    #[test]
    fn profile_agent_card_uses_application_profile_descriptor() {
        let mut manifest = crate::instance_profile::InstanceProfile::default_instance().manifest;
        manifest.id = "travel.planner".into();
        manifest.name = "Travel Planner".into();
        manifest.description = Some("Plans trips".into());
        manifest.capabilities.tags = vec!["travel".into()];
        manifest.capabilities.intents = vec!["plan-trip".into()];
        let profile = crate::application::ApplicationProfileInfo::from(&manifest);
        let card = stdio_agent_card(&profile);
        assert_eq!(card.name, "Travel Planner");
        assert_eq!(card.skills[0].id, "plan-trip");
        assert_eq!(card.skills[0].tags, vec!["travel"]);
    }

    #[test]
    fn public_binding_requires_token() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(!is_loopback_host("0.0.0.0"));
    }

    #[tokio::test]
    async fn json_task_store_survives_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tasks.json");
        let store = JsonTaskStore::load(path.clone()).unwrap();
        let task = Task {
            id: "task-1".to_string(),
            context_id: "context-1".to_string(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        store.create(task.clone()).await.unwrap();
        drop(store);

        let reloaded = JsonTaskStore::load(path).unwrap();
        assert_eq!(reloaded.get("task-1").await.unwrap(), Some(task));
    }

    #[tokio::test]
    async fn agent_card_is_public_but_protocol_routes_require_token() {
        let directory = tempfile::tempdir().unwrap();
        let sessions = Arc::new(Mutex::new(
            SessionRuntime::load_path(directory.path().join("sessions.json")).unwrap(),
        ));
        let (web_chat, _receiver) = WebChatHandle::channel();
        let app = router(
            web_chat,
            sessions,
            "default".to_string(),
            JsonTaskStore::load(directory.path().join("tasks.json")).unwrap(),
            "http://127.0.0.1:8788",
            Some("secret"),
        );

        let card = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/agent-card.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(card.status(), StatusCode::OK);

        let protected = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/message:send")
                    .header(header::CONTENT_TYPE, "application/a2a+json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            protected.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );
    }

    #[tokio::test]
    async fn streaming_message_runs_through_dispatcher_and_persists_task() {
        let directory = tempfile::tempdir().unwrap();
        let sessions = Arc::new(Mutex::new(
            SessionRuntime::load_path(directory.path().join("sessions.json")).unwrap(),
        ));
        let (web_chat, mut receiver) = WebChatHandle::channel();
        tokio::spawn(async move {
            let Some(WebChatCommand::Run {
                run_id,
                session_id,
                runtime,
                response,
                ..
            }) = receiver.recv().await
            else {
                panic!("expected A2A run command");
            };
            assert!(!run_id.is_empty());
            assert_eq!(runtime.platform.as_deref(), Some(A2A_CHANNEL));
            assert_eq!(runtime.agent_id.as_deref(), Some("explorer"));
            let (events, events_rx) = tokio::sync::mpsc::channel(8);
            assert!(response.send(Ok(WebRun { events: events_rx })).is_ok());
            for (sequence, event, data) in [
                (0, "run_started", None),
                (1, "text_delta", Some(serde_json::json!({"text": "hello "}))),
                (2, "text_delta", Some(serde_json::json!({"text": "world"}))),
                (
                    3,
                    "run_finished",
                    Some(serde_json::json!({"status": "completed"})),
                ),
            ] {
                events
                    .send(ChatEventV1 {
                        version: 1,
                        event: event.to_string(),
                        run_id: run_id.clone(),
                        session_id: session_id.clone(),
                        sequence,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        data,
                    })
                    .await
                    .unwrap();
            }
        });

        let app = router(
            web_chat,
            Arc::clone(&sessions),
            "default".to_string(),
            JsonTaskStore::load(directory.path().join("tasks.json")).unwrap(),
            "http://127.0.0.1:8788",
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let request = a2a::SendMessageRequest {
            message: Message {
                message_id: "message-1".to_string(),
                context_id: Some("context-1".to_string()),
                task_id: None,
                role: Role::User,
                parts: vec![Part::text("say hello")],
                metadata: Some(HashMap::from([(
                    INVOCATION_EXTENSION.to_string(),
                    serde_json::json!({"agentId": "explorer"}),
                )])),
                extensions: None,
                reference_task_ids: None,
            },
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let response = client
            .post(format!("http://{address}/message:stream"))
            .header(header::CONTENT_TYPE.as_str(), "application/a2a+json")
            .header("A2A-Version", "1.0")
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("TASK_STATE_WORKING"), "{body}");
        assert!(body.contains("artifactUpdate"), "{body}");
        assert!(body.contains(ACTIVITY_ARTIFACT_ID), "{body}");
        assert!(body.contains("hello "), "{body}");
        assert!(body.contains("world"), "{body}");
        assert!(body.contains("TASK_STATE_COMPLETED"), "{body}");

        let assigned_task_id = body
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .filter_map(|line| serde_json::from_str::<StreamResponse>(line.trim()).ok())
            .find_map(|event| match event {
                StreamResponse::StatusUpdate(update) => Some(update.task_id),
                StreamResponse::ArtifactUpdate(update) => Some(update.task_id),
                StreamResponse::Task(task) => Some(task.id),
                StreamResponse::Message(_) => None,
            })
            .expect("server assigned a task id");
        assert_ne!(assigned_task_id, "message-1");
        let task_response = client
            .get(format!("http://{address}/tasks/{assigned_task_id}"))
            .header("A2A-Version", "1.0")
            .send()
            .await
            .unwrap();
        assert_eq!(task_response.status(), StatusCode::OK);
        let task: Task = task_response.json().await.unwrap();
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(
            task.artifacts.unwrap()[0].parts[0].as_text(),
            Some("hello world")
        );
        assert!(sessions
            .lock()
            .await
            .channel_session_id(A2A_CHANNEL, "context-1")
            .is_some());
        server.abort();
    }

    #[tokio::test]
    async fn cancel_waits_for_runtime_dispatcher_before_reporting_canceled() {
        use futures::StreamExt;

        let directory = tempfile::tempdir().unwrap();
        let sessions = Arc::new(Mutex::new(
            SessionRuntime::load_path(directory.path().join("sessions.json")).unwrap(),
        ));
        let (web_chat, mut receiver) = WebChatHandle::channel();
        let executor = RemiA2aExecutor::new(
            web_chat,
            sessions,
            "default".to_string(),
            Arc::new(RwLock::new(HashMap::new())),
        );
        let context = ExecutorContext {
            message: None,
            task_id: "task-cancel".to_string(),
            stored_task: None,
            context_id: "context-cancel".to_string(),
            metadata: None,
            user: None,
            service_params: HashMap::new(),
            tenant: None,
        };
        let mut stream = executor.cancel(context);
        let event = tokio::spawn(async move { stream.next().await });

        let Some(WebChatCommand::Cancel { run_id, response }) = receiver.recv().await else {
            panic!("expected runtime cancel command");
        };
        assert_eq!(run_id, "task-cancel");
        response.send(true).unwrap();

        let response = event.await.unwrap().unwrap().unwrap();
        let StreamResponse::StatusUpdate(update) = response else {
            panic!("expected canceled status update");
        };
        assert_eq!(update.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn active_task_controls_cross_real_http_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let sessions = Arc::new(Mutex::new(
            SessionRuntime::load_path(directory.path().join("sessions.json")).unwrap(),
        ));
        let (web_chat, mut receiver) = WebChatHandle::channel();
        let (assigned_tx, assigned_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Some(WebChatCommand::Run {
                run_id,
                session_id,
                response,
                ..
            }) = receiver.recv().await
            else {
                panic!("expected A2A run command");
            };
            let (events, events_rx) = tokio::sync::mpsc::channel(16);
            assert!(response.send(Ok(WebRun { events: events_rx })).is_ok());
            events
                .send(ChatEventV1 {
                    version: 1,
                    event: "run_started".to_string(),
                    run_id: run_id.clone(),
                    session_id: session_id.clone(),
                    sequence: 0,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    data: None,
                })
                .await
                .unwrap();
            assigned_tx
                .send((run_id.clone(), session_id.clone()))
                .unwrap();

            let Some(WebChatCommand::Steer {
                run_id: steer_run_id,
                session_id: steer_session_id,
                content,
                response,
            }) = receiver.recv().await
            else {
                panic!("expected steer command");
            };
            assert_eq!(steer_run_id, run_id);
            assert_eq!(steer_session_id, session_id);
            assert!(format!("{content:?}").contains("focus on tests"));
            response.send(Ok(())).unwrap();

            let Some(WebChatCommand::DecideApproval {
                approval_id,
                decision,
                response,
            }) = receiver.recv().await
            else {
                panic!("expected approval command");
            };
            assert_eq!(approval_id, "approval-1");
            assert_eq!(decision, bot_core::ToolApprovalDecision::AllowOnce);
            response
                .send(Some(bot_core::ToolApprovalRequest {
                    id: approval_id,
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    tool_call_id: "call-1".to_string(),
                    tool_name: "bash".to_string(),
                    risk: bot_core::ToolRiskLevel::High,
                    args_summary: "test".to_string(),
                    command_key: None,
                    model_review_reason: None,
                    platform: Some(A2A_CHANNEL.to_string()),
                    app_id: None,
                    review: None,
                }))
                .unwrap();

            let Some(WebChatCommand::AnswerUserQuestion {
                question_id,
                answer,
                response,
            }) = receiver.recv().await
            else {
                panic!("expected user-question command");
            };
            assert_eq!(question_id, "question-1");
            assert_eq!(answer.answer_text.as_deref(), Some("yes"));
            response
                .send(Some(bot_core::UserQuestionRequest {
                    id: question_id,
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    app_id: None,
                    tool_call_id: "call-2".to_string(),
                    question: "continue?".to_string(),
                    reason: None,
                    options: Vec::new(),
                    allow_free_text: true,
                    placeholder: None,
                    default_option_id: None,
                    created_at: chrono::Utc::now().to_rfc3339(),
                }))
                .unwrap();

            let Some(WebChatCommand::Cancel {
                run_id: cancel_run_id,
                response,
            }) = receiver.recv().await
            else {
                panic!("expected cancel command");
            };
            assert_eq!(cancel_run_id, run_id);
            response.send(true).unwrap();
            for (sequence, event, data) in [
                (1, "cancelled", None),
                (
                    2,
                    "run_finished",
                    Some(serde_json::json!({"status": "cancelled"})),
                ),
            ] {
                events
                    .send(ChatEventV1 {
                        version: 1,
                        event: event.to_string(),
                        run_id: run_id.clone(),
                        session_id: session_id.clone(),
                        sequence,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        data,
                    })
                    .await
                    .unwrap();
            }
        });

        let app = router(
            web_chat,
            sessions,
            "default".to_string(),
            JsonTaskStore::load(directory.path().join("tasks.json")).unwrap(),
            "http://127.0.0.1:8788",
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let stream_client = client.clone();
        let stream = tokio::spawn(async move {
            stream_client
                .post(format!("http://{address}/message:stream"))
                .header(header::CONTENT_TYPE.as_str(), "application/a2a+json")
                .header("A2A-Version", "1.0")
                .json(&a2a::SendMessageRequest {
                    message: Message {
                        message_id: "control-message".to_string(),
                        context_id: Some("control-context".to_string()),
                        task_id: None,
                        role: Role::User,
                        parts: vec![Part::text("wait for controls")],
                        metadata: None,
                        extensions: Some(vec![
                            ACTIVITY_EXTENSION.to_string(),
                            INTERACTIVE_EXTENSION.to_string(),
                            HANDOFF_EXTENSION.to_string(),
                        ]),
                        reference_task_ids: None,
                    },
                    configuration: None,
                    metadata: None,
                    tenant: None,
                })
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        let (task_id, _) = assigned_rx.await.unwrap();

        let steer = client
            .post(format!("http://{address}/tasks/{task_id}/remi:steer"))
            .json(&serde_json::json!({"content": bot_core::Content::text("focus on tests")}))
            .send()
            .await
            .unwrap();
        assert_eq!(steer.status(), StatusCode::ACCEPTED);

        let approval = client
            .post(format!("http://{address}/tasks/{task_id}/remi:approval"))
            .json(&serde_json::json!({
                "approval_id": "approval-1",
                "decision": "allow_once"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(approval.status(), StatusCode::ACCEPTED);

        let question = client
            .post(format!("http://{address}/tasks/{task_id}/remi:question"))
            .json(&serde_json::json!({
                "question_id": "question-1",
                "response": {
                    "question_id": "question-1",
                    "status": "answered",
                    "answer_text": "yes"
                }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(question.status(), StatusCode::ACCEPTED);

        let cancel = client
            .post(format!("http://{address}/tasks/{task_id}:cancel"))
            .header("A2A-Version", "1.0")
            .send()
            .await
            .unwrap();
        assert_eq!(cancel.status(), StatusCode::OK);
        let body = stream.await.unwrap();
        assert!(body.contains("TASK_STATE_CANCELED"), "{body}");
        assert!(!body.contains("TASK_STATE_COMPLETED"), "{body}");
        let persisted: Task = client
            .get(format!("http://{address}/tasks/{task_id}"))
            .header("A2A-Version", "1.0")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(persisted.status.state, TaskState::Canceled);
        server.abort();
    }

    #[tokio::test]
    async fn json_task_store_terminal_state_is_monotonic() {
        let directory = tempfile::tempdir().unwrap();
        let store = JsonTaskStore::load(directory.path().join("tasks.json")).unwrap();
        let task = Task {
            id: "terminal-task".to_string(),
            context_id: "terminal-context".to_string(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        store.create(task.clone()).await.unwrap();
        let mut canceled = task.clone();
        canceled.status.state = TaskState::Canceled;
        store.update(canceled).await.unwrap();
        let mut stale = task;
        stale.artifacts = Some(vec![Artifact {
            artifact_id: "late".to_string(),
            name: None,
            description: None,
            parts: vec![Part::text("late activity")],
            metadata: None,
            extensions: None,
        }]);
        store.update(stale).await.unwrap();

        let persisted = store.get("terminal-task").await.unwrap().unwrap();
        assert_eq!(persisted.status.state, TaskState::Canceled);
        assert_eq!(persisted.artifacts.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn activity_and_failure_events_cross_real_sse_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let sessions = Arc::new(Mutex::new(
            SessionRuntime::load_path(directory.path().join("sessions.json")).unwrap(),
        ));
        let (web_chat, mut receiver) = WebChatHandle::channel();
        tokio::spawn(async move {
            let Some(WebChatCommand::Run {
                run_id,
                session_id,
                response,
                ..
            }) = receiver.recv().await
            else {
                panic!("expected A2A run command");
            };
            let (events, events_rx) = tokio::sync::mpsc::channel(16);
            assert!(response.send(Ok(WebRun { events: events_rx })).is_ok());
            for (sequence, event, data) in [
                (
                    0,
                    "thinking_delta",
                    Some(serde_json::json!({"text": "plan"})),
                ),
                (
                    1,
                    "tool_call_started",
                    Some(serde_json::json!({"call_id": "call-1", "tool_name": "rg"})),
                ),
                (
                    2,
                    "tool_call_arguments_delta",
                    Some(serde_json::json!({"call_id": "call-1", "delta": "{\"pattern\":"})),
                ),
                (
                    3,
                    "tool_call_result",
                    Some(
                        serde_json::json!({"call_id": "call-1", "tool_name": "rg", "result": "match"}),
                    ),
                ),
                (
                    4,
                    "error",
                    Some(serde_json::json!({"message": "delegate exploded"})),
                ),
                (
                    5,
                    "run_finished",
                    Some(serde_json::json!({"status": "error"})),
                ),
            ] {
                events
                    .send(ChatEventV1 {
                        version: 1,
                        event: event.to_string(),
                        run_id: run_id.clone(),
                        session_id: session_id.clone(),
                        sequence,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        data,
                    })
                    .await
                    .unwrap();
            }
        });
        let app = router(
            web_chat,
            sessions,
            "default".to_string(),
            JsonTaskStore::load(directory.path().join("tasks.json")).unwrap(),
            "http://127.0.0.1:8788",
            None,
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = reqwest::Client::new()
            .post(format!("http://{address}/message:stream"))
            .header(header::CONTENT_TYPE.as_str(), "application/a2a+json")
            .header("A2A-Version", "1.0")
            .json(&a2a::SendMessageRequest {
                message: Message {
                    message_id: "failure-message".to_string(),
                    context_id: Some("failure-context".to_string()),
                    task_id: None,
                    role: Role::User,
                    parts: vec![Part::text("fail after tool activity")],
                    metadata: None,
                    extensions: Some(vec![ACTIVITY_EXTENSION.to_string()]),
                    reference_task_ids: None,
                },
                configuration: None,
                metadata: None,
                tenant: None,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        for expected in [
            "thinking_delta",
            "tool_call_started",
            "tool_call_arguments_delta",
            "tool_call_result",
            "delegate exploded",
            "TASK_STATE_FAILED",
        ] {
            assert!(body.contains(expected), "missing {expected}: {body}");
        }
        assert!(!body.contains("TASK_STATE_COMPLETED"), "{body}");
        server.abort();
    }
}
