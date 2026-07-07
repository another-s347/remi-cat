use std::sync::Arc;

use async_stream::stream;
use futures::{stream::BoxStream, Stream, StreamExt};
use remi_agentloop::prelude::{AgentError, Tool, ToolContext, ToolOutput, ToolResult};
use remi_agentloop::types::{
    ResumePayload, RunId, SubSessionEvent, SubSessionEventPayload, ThreadId,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::approval::{
    ApprovalEvent, ApprovalResolution, ApprovalWait, ToolApprovalDecision, ToolApprovalManager,
    ToolApprovalRequest,
};

use super::backend::{
    AcpApprovalDecision, AcpBackend, AcpBoundChannel, AcpToolRequest, AcpToolTaskEvent,
    AcpToolTaskStatus,
};

const DEFAULT_ACP_WAIT_MS: u64 = 30_000;

pub struct AcpChatTool {
    backend: Arc<AcpBackend>,
    approval_manager: Arc<ToolApprovalManager>,
    name: String,
    description: String,
}

impl AcpChatTool {
    pub fn new(
        backend: Arc<AcpBackend>,
        approval_manager: Arc<ToolApprovalManager>,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            backend,
            approval_manager,
            name: name.into(),
            description: description.into(),
        }
    }

    pub fn codex(backend: Arc<AcpBackend>, approval_manager: Arc<ToolApprovalManager>) -> Self {
        Self::new(
            backend,
            approval_manager,
            "codex",
            "Open or resume the configured Codex ACP adapter session. Provide `message` to start work; the tool streams the ACP sub-session until it completes. Optionally pass `session_id` to resume an ACP session or `title` for a new session.",
        )
    }
}

#[derive(Debug, Deserialize)]
struct AcpChatArguments {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    wait_ms: Option<u64>,
    #[serde(default)]
    startup_args: Vec<String>,
}

impl Tool for AcpChatTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The user message to send to the configured ACP client." },
                "session_id": { "type": "string", "description": "Optional ACP session id to resume. The tool result also returns the session_id to reuse later." },
                "title": { "type": "string", "description": "Optional title for a newly created ACP session." },
                "task_id": { "type": "string", "description": "Compatibility-only: poll a background task created by an older running result." },
                "action": { "type": "string", "enum": ["poll"], "description": "Compatibility-only action for an existing task_id." },
                "wait_ms": { "type": "integer", "description": "Deprecated for initial Codex runs; the tool now streams until completion. Poll actions still return the current task status." },
                "startup_args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Deprecated. Configure local adapter argv in the ACP profile instead."
                }
            }
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let backend = Arc::clone(&self.backend);
        let approval_manager = Arc::clone(&self.approval_manager);
        let tool_name = self.name.clone();
        let ctx = ctx.clone();
        async move {
            let args: AcpChatArguments = serde_json::from_value(arguments).map_err(|_| {
                AgentError::tool(
                    &tool_name,
                    "expected {message?, session_id?, title?, task_id?, action?, wait_ms?, startup_args?}",
                )
            })?;
            if let Some(task_id) = args
                .task_id
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                let action = args.action.as_deref().unwrap_or("poll");
                if action != "poll" {
                    return Err(AgentError::tool(&tool_name, "unsupported ACP task action"));
                }
                let status = backend.poll_tool_task(task_id).await;
                let preview = serde_json::to_string_pretty(&status)
                    .map_err(|err| AgentError::tool(&tool_name, err.to_string()))?;
                return Ok(ToolResult::Output(status_stream(status, preview)));
            }

            let message = args
                .message
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| AgentError::tool(&tool_name, "missing message or task_id"))?;
            let current_channel = current_bound_channel(&tool_name, &ctx)?;
            let prepared = backend
                .prepare_tool_turn(AcpToolRequest {
                    message,
                    session_id: args.session_id,
                    title: args.title,
                    current_channel: Some(current_channel),
                    startup_args: args.startup_args,
                })
                .await
                .map_err(|err| AgentError::tool(&tool_name, err.to_string()))?;

            let sub_thread_id = ThreadId(prepared.sub_session_id.clone());
            let sub_run_id = RunId(Uuid::new_v4().to_string());
            let title = prepared.title.clone();
            let _wait_ms = args.wait_ms.unwrap_or(DEFAULT_ACP_WAIT_MS);

            Ok(ToolResult::Output(
                stream! {
                    yield ToolOutput::SubSession(SubSessionEvent::new(
                        "",
                        sub_thread_id.clone(),
                        sub_run_id.clone(),
                        "acp",
                        title.clone(),
                        1,
                        SubSessionEventPayload::Start,
                    ));
                    yield ToolOutput::SubSession(SubSessionEvent::new(
                        "",
                        sub_thread_id.clone(),
                        sub_run_id.clone(),
                        "acp",
                        title.clone(),
                        1,
                        SubSessionEventPayload::ThinkingStart,
                    ));
                    let mut spawned = backend
                        .spawn_prepared_tool_turn_with_events(prepared.clone())
                        .await;
                    let decision_tx = spawned.decisions.clone();
                    let mut saw_output = false;
                    let mut accumulated_output = String::new();
                    while let Some(event) = spawned.events.recv().await {
                        match event {
                            AcpToolTaskEvent::Delta(content) => {
                                saw_output = true;
                                accumulated_output.push_str(&content);
                                yield acp_task_event_output(
                                    AcpToolTaskEvent::Delta(content),
                                    &sub_thread_id,
                                    &sub_run_id,
                                    title.clone(),
                                );
                            }
                            AcpToolTaskEvent::ApprovalRequested(request) => {
                                for output in handle_acp_approval_request(
                                    request,
                                    &approval_manager,
                                    decision_tx.clone(),
                                    &ctx,
                                    &sub_thread_id,
                                    &sub_run_id,
                                    title.clone(),
                                ).await {
                                    yield output;
                                }
                            }
                            AcpToolTaskEvent::ApprovalUpdated(request) => {
                                let request = normalize_acp_approval_request(request, &ctx);
                                yield crate::tool_approval_updated_marker(
                                    &sub_thread_id,
                                    &sub_run_id,
                                    &request,
                                );
                            }
                            AcpToolTaskEvent::ApprovalResolved { request, decision } => {
                                let request = normalize_acp_approval_request(request, &ctx);
                                yield crate::tool_approval_resolved_marker(
                                    &sub_thread_id,
                                    &sub_run_id,
                                    &request,
                                    decision,
                                );
                                if decision == ToolApprovalDecision::Deny {
                                    backend.abort_tool_task(&spawned.task_id).await;
                                    yield ToolOutput::SubSession(SubSessionEvent::new(
                                        "",
                                        sub_thread_id.clone(),
                                        sub_run_id.clone(),
                                        "acp",
                                        title.clone(),
                                        1,
                                        SubSessionEventPayload::Done { final_output: None },
                                    ));
                                    return;
                                }
                            }
                            event => {
                                yield acp_task_event_output(
                                    event,
                                    &sub_thread_id,
                                    &sub_run_id,
                                    title.clone(),
                                );
                            }
                        }
                    }
                    let status = wait_for_finished_status(&backend, &spawned.task_id).await;
                    let preview = serde_json::to_string_pretty(&status)
                        .unwrap_or_else(|_| serde_json::to_string(&status).unwrap_or_else(|_| status.status.clone()));
                    for output in status_sub_session_outputs(
                        status.clone(),
                        &sub_thread_id,
                        &sub_run_id,
                        title.clone(),
                        !saw_output,
                        if accumulated_output.trim().is_empty() {
                            None
                        } else {
                            Some(accumulated_output.clone())
                        },
                    ) {
                        yield output;
                    }
                    yield ToolOutput::text(preview);
                }
                .boxed(),
            ))
        }
    }
}

fn status_stream(status: AcpToolTaskStatus, preview: String) -> BoxStream<'static, ToolOutput> {
    stream! {
        let sub_thread_id = ThreadId(status.sub_session_id.clone());
        let sub_run_id = RunId(Uuid::new_v4().to_string());
        let title = Some("ACP".to_string());
        for output in status_sub_session_outputs(status.clone(), &sub_thread_id, &sub_run_id, title, true, None) {
            yield output;
        }
        yield ToolOutput::text(preview);
    }
    .boxed()
}

async fn wait_for_finished_status(backend: &Arc<AcpBackend>, task_id: &str) -> AcpToolTaskStatus {
    loop {
        let status = backend.poll_tool_task(task_id).await;
        if status.status != "running" {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn status_sub_session_outputs(
    status: AcpToolTaskStatus,
    sub_thread_id: &ThreadId,
    sub_run_id: &RunId,
    title: Option<String>,
    include_response_delta: bool,
    final_output_override: Option<String>,
) -> Vec<ToolOutput> {
    let mut outputs = Vec::new();
    if let Some(response) = status.response.clone() {
        if include_response_delta {
            outputs.push(ToolOutput::SubSession(SubSessionEvent::new(
                "",
                sub_thread_id.clone(),
                sub_run_id.clone(),
                "acp",
                title.clone(),
                1,
                SubSessionEventPayload::Delta {
                    content: response.reply.clone(),
                },
            )));
        }
        outputs.push(ToolOutput::SubSession(SubSessionEvent::new(
            "",
            sub_thread_id.clone(),
            sub_run_id.clone(),
            "acp",
            title,
            1,
            SubSessionEventPayload::Done {
                final_output: Some(final_output_override.unwrap_or(response.final_summary.clone())),
            },
        )));
    } else if let Some(error) = status.error.clone() {
        outputs.push(ToolOutput::SubSession(SubSessionEvent::new(
            "",
            sub_thread_id.clone(),
            sub_run_id.clone(),
            "acp",
            title,
            1,
            SubSessionEventPayload::Error { message: error },
        )));
    } else if status.status == "running" {
        outputs.push(ToolOutput::SubSession(SubSessionEvent::new(
            "",
            sub_thread_id.clone(),
            sub_run_id.clone(),
            "acp",
            title,
            1,
            SubSessionEventPayload::ThinkingEnd {
                content: status
                    .poll_hint
                    .clone()
                    .unwrap_or_else(|| "ACP task is still running.".to_string()),
            },
        )));
    }
    outputs
}

fn acp_task_event_output(
    event: AcpToolTaskEvent,
    sub_thread_id: &ThreadId,
    sub_run_id: &RunId,
    title: Option<String>,
) -> ToolOutput {
    let payload = match event {
        AcpToolTaskEvent::Delta(content) => SubSessionEventPayload::Delta { content },
        AcpToolTaskEvent::Thinking(content) => SubSessionEventPayload::ThinkingEnd { content },
        AcpToolTaskEvent::ToolCallStart { id, name } => {
            SubSessionEventPayload::ToolCallStart { id, name }
        }
        AcpToolTaskEvent::ToolDelta { id, name, delta } => {
            SubSessionEventPayload::ToolDelta { id, name, delta }
        }
        AcpToolTaskEvent::ToolResult { id, name, result } => {
            SubSessionEventPayload::ToolResult { id, name, result }
        }
        AcpToolTaskEvent::ApprovalRequested(request)
        | AcpToolTaskEvent::ApprovalUpdated(request)
        | AcpToolTaskEvent::ApprovalResolved { request, .. } => SubSessionEventPayload::Error {
            message: format!(
                "ACP approval event was not handled before rendering: {}",
                request.id
            ),
        },
    };
    ToolOutput::SubSession(SubSessionEvent::new(
        "",
        sub_thread_id.clone(),
        sub_run_id.clone(),
        "acp",
        title,
        1,
        payload,
    ))
}

async fn handle_acp_approval_request(
    request: ToolApprovalRequest,
    approval_manager: &Arc<ToolApprovalManager>,
    decision_tx: Option<tokio::sync::mpsc::UnboundedSender<AcpApprovalDecision>>,
    ctx: &ToolContext,
    sub_thread_id: &ThreadId,
    sub_run_id: &RunId,
    title: Option<String>,
) -> Vec<ToolOutput> {
    let request = normalize_acp_approval_request(request, ctx);
    let mut outputs = Vec::new();

    if decision_tx.is_none() {
        outputs.push(crate::tool_approval_requested_marker(
            sub_thread_id,
            sub_run_id,
            &request,
        ));
        outputs.push(crate::tool_approval_resolved_marker(
            sub_thread_id,
            sub_run_id,
            &request,
            ToolApprovalDecision::Deny,
        ));
        outputs.push(acp_sub_session_error(
            sub_thread_id,
            sub_run_id,
            title,
            "ACP approval requested, but this ACP backend does not expose a decision channel.",
        ));
        return outputs;
    }

    let (wait, events) = approval_manager.start_request(request.clone()).await;
    let mut current_request = request;
    for event in events {
        match event {
            ApprovalEvent::Requested(request) => {
                current_request = request.clone();
                outputs.push(crate::tool_approval_requested_marker(
                    sub_thread_id,
                    sub_run_id,
                    &request,
                ));
            }
            ApprovalEvent::Updated(request) => {
                current_request = request.clone();
                outputs.push(crate::tool_approval_updated_marker(
                    sub_thread_id,
                    sub_run_id,
                    &request,
                ));
            }
            ApprovalEvent::Resolved { request, decision } => {
                current_request = request.clone();
                outputs.push(crate::tool_approval_resolved_marker(
                    sub_thread_id,
                    sub_run_id,
                    &request,
                    decision,
                ));
            }
        }
    }

    let decision = match wait {
        ApprovalWait::Immediate(ApprovalResolution::Approved) => {
            ToolApprovalDecision::AllowRiskLevelSession
        }
        ApprovalWait::Immediate(ApprovalResolution::Denied) => ToolApprovalDecision::Deny,
        ApprovalWait::Pending(rx) => {
            let should_wait = matches!(
                current_request.platform.as_deref(),
                Some("web" | "tui" | "feishu")
            );
            if should_wait {
                rx.await.unwrap_or(ToolApprovalDecision::Deny)
            } else {
                ToolApprovalDecision::Deny
            }
        }
    };

    let (_, event) = approval_manager
        .finish_request(&current_request, decision)
        .await;
    if let ApprovalEvent::Resolved { request, decision } = event {
        current_request = request.clone();
        outputs.push(crate::tool_approval_resolved_marker(
            sub_thread_id,
            sub_run_id,
            &request,
            decision,
        ));
    }

    if let Some(tx) = decision_tx {
        if tx
            .send(AcpApprovalDecision {
                approval_id: current_request.id.clone(),
                decision,
            })
            .is_err()
        {
            outputs.push(acp_sub_session_error(
                sub_thread_id,
                sub_run_id,
                title,
                "ACP approval decision could not be delivered; the ACP session is no longer accepting decisions.",
            ));
        }
    }
    outputs
}

fn normalize_acp_approval_request(
    mut request: ToolApprovalRequest,
    ctx: &ToolContext,
) -> ToolApprovalRequest {
    if let Some(thread_id) = &ctx.thread_id {
        request.session_id = thread_id.0.clone();
    }
    if request.run_id.trim().is_empty() {
        request.run_id = ctx.run_id.0.clone();
    }
    if request.platform.as_deref().is_none_or(str::is_empty) {
        request.platform = ctx
            .metadata
            .as_ref()
            .and_then(|value| value.get("platform"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| Some("cli".to_string()));
    }
    if request.command_key.is_none() {
        request.command_key = Some(crate::approval::command_key(
            &request.tool_name,
            &serde_json::json!({ "summary": request.args_summary }),
        ));
    }
    request
}

fn acp_sub_session_error(
    sub_thread_id: &ThreadId,
    sub_run_id: &RunId,
    title: Option<String>,
    message: impl Into<String>,
) -> ToolOutput {
    ToolOutput::SubSession(SubSessionEvent::new(
        "",
        sub_thread_id.clone(),
        sub_run_id.clone(),
        "acp",
        title,
        1,
        SubSessionEventPayload::Error {
            message: message.into(),
        },
    ))
}

fn current_bound_channel(
    tool_name: &str,
    ctx: &ToolContext,
) -> Result<AcpBoundChannel, AgentError> {
    let platform = ctx
        .metadata
        .as_ref()
        .and_then(|value| value.get("platform"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("cli");
    let channel_id = ctx
        .thread_id
        .as_ref()
        .map(|value| value.0.trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::tool(tool_name, "missing thread/channel id in tool context"))?;
    Ok(AcpBoundChannel {
        platform: platform.to_string(),
        channel_id: channel_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::{current_bound_channel, status_sub_session_outputs, AcpChatTool};
    use crate::acp::backend::{AcpBackend, AcpToolResponse, AcpToolTaskStatus};
    use crate::approval::ToolApprovalManager;
    use remi_agentloop::prelude::{
        AgentConfig, SubSessionEventPayload, ThreadId, Tool, ToolContext, ToolOutput,
    };
    use remi_agentloop::types::RunId;
    use std::sync::{Arc, RwLock};

    fn tool_context(metadata: Option<serde_json::Value>) -> ToolContext {
        ToolContext {
            config: AgentConfig::default(),
            thread_id: Some(serde_json::from_value(serde_json::json!("thread-1")).unwrap()),
            run_id: serde_json::from_value(serde_json::json!("run-1")).unwrap(),
            metadata,
            cancel: None,
            user_state: Arc::new(RwLock::new(serde_json::Value::Null)),
        }
    }

    #[test]
    fn bound_channel_uses_metadata_platform() {
        let channel = current_bound_channel(
            "codex",
            &tool_context(Some(serde_json::json!({
                "platform": "feishu"
            }))),
        )
        .unwrap();

        assert_eq!(channel.platform, "feishu");
        assert_eq!(channel.channel_id, "thread-1");
    }

    #[test]
    fn bound_channel_defaults_to_cli_without_metadata_platform() {
        let channel = current_bound_channel("codex", &tool_context(None)).unwrap();

        assert_eq!(channel.platform, "cli");
        assert_eq!(channel.channel_id, "thread-1");
    }

    #[test]
    fn codex_tool_exposes_session_id_contract() {
        let tempdir = tempfile::tempdir().unwrap();
        let tool = AcpChatTool::codex(
            Arc::new(AcpBackend::new(tempdir.path().to_path_buf(), None)),
            ToolApprovalManager::new(),
        );
        let schema = tool.parameters_schema();

        assert_eq!(tool.name(), "codex");
        assert!(tool.description().contains("session_id"));
        assert!(schema["properties"].get("session_id").is_some());
    }

    #[test]
    fn status_outputs_use_accumulated_stream_as_final_output() {
        let outputs = status_sub_session_outputs(
            AcpToolTaskStatus {
                status: "completed".to_string(),
                task_id: "task-1".to_string(),
                session_id: "session-1".to_string(),
                sub_session_id: "sub-1".to_string(),
                poll_hint: None,
                response: Some(AcpToolResponse {
                    session_id: "session-1".to_string(),
                    reply: "fallback reply".to_string(),
                    final_summary: "fallback summary".to_string(),
                }),
                error: None,
            },
            &ThreadId("sub-1".to_string()),
            &RunId("run-1".to_string()),
            Some("Codex ACP".to_string()),
            false,
            Some("part one, part two".to_string()),
        );

        let final_output = outputs.into_iter().find_map(|output| {
            let ToolOutput::SubSession(event) = output else {
                return None;
            };
            match event.payload {
                SubSessionEventPayload::Done { final_output } => final_output,
                _ => None,
            }
        });
        assert_eq!(final_output.as_deref(), Some("part one, part two"));
    }
}
