use std::sync::Arc;

use async_stream::stream;
use futures::{stream::BoxStream, Stream, StreamExt};
use remi_agentloop::prelude::{AgentError, Tool, ToolContext, ToolOutput, ToolResult};
use remi_agentloop::types::{
    ResumePayload, RunId, SubSessionEvent, SubSessionEventPayload, ThreadId,
};
use serde::Deserialize;
use uuid::Uuid;

use super::backend::{AcpBackend, AcpBoundChannel, AcpToolRequest, AcpToolTaskStatus};

const DEFAULT_CODEX_WAIT_MS: u64 = 30_000;

pub struct AcpChatTool {
    backend: Arc<AcpBackend>,
    name: &'static str,
    description: &'static str,
}

impl AcpChatTool {
    pub fn codex(backend: Arc<AcpBackend>) -> Self {
        Self {
            backend,
            name: "codex",
            description: "Open, resume, or poll a Codex ACP session. Provide `message` to start work; if the result is running, call again with `task_id` and `action=\"poll\"`. Optionally pass `session_id` to resume an ACP session or `title` for a new session.",
        }
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
}

impl Tool for AcpChatTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The user message to send to Codex/ACP." },
                "session_id": { "type": "string", "description": "Optional ACP session id to resume. The tool result also returns the session_id to reuse later." },
                "title": { "type": "string", "description": "Optional title for a newly created ACP session." },
                "task_id": { "type": "string", "description": "Background Codex task id returned by a prior running result." },
                "action": { "type": "string", "enum": ["poll"], "description": "Action for an existing task_id. Defaults to poll when task_id is provided." },
                "wait_ms": { "type": "integer", "description": "How long to wait for initial completion before returning a running task_id. Defaults to 30000. This does not kill Codex." }
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
        let tool_name = self.name;
        let ctx = ctx.clone();
        async move {
            let args: AcpChatArguments = serde_json::from_value(arguments).map_err(|_| {
                AgentError::tool(
                    tool_name,
                    "expected {message?, session_id?, title?, task_id?, action?, wait_ms?}",
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
                    return Err(AgentError::tool(tool_name, "unsupported codex task action"));
                }
                let status = backend.poll_tool_task(task_id).await;
                let preview = serde_json::to_string_pretty(&status)
                    .map_err(|err| AgentError::tool(tool_name, err.to_string()))?;
                return Ok(ToolResult::Output(status_stream(status, preview)));
            }

            let message = args
                .message
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| AgentError::tool(tool_name, "missing message or task_id"))?;
            let current_channel = current_bound_channel(tool_name, &ctx)?;
            let prepared = backend
                .prepare_tool_turn(AcpToolRequest {
                    message,
                    session_id: args.session_id,
                    title: args.title,
                    current_channel: Some(current_channel),
                })
                .await
                .map_err(|err| AgentError::tool(tool_name, err.to_string()))?;

            let sub_thread_id = ThreadId(prepared.sub_session_id.clone());
            let sub_run_id = RunId(Uuid::new_v4().to_string());
            let title = prepared.title.clone();
            let wait_ms = args.wait_ms.unwrap_or(DEFAULT_CODEX_WAIT_MS);
            let status = backend
                .spawn_prepared_tool_turn(prepared.clone(), wait_ms)
                .await;
            let preview = serde_json::to_string_pretty(&status)
                .map_err(|err| AgentError::tool(tool_name, err.to_string()))?;

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
                    if let Some(response) = status.response.clone() {
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            "",
                            sub_thread_id.clone(),
                            sub_run_id.clone(),
                            "acp",
                            title.clone(),
                            1,
                            SubSessionEventPayload::Delta {
                                content: response.reply.clone(),
                            },
                        ));
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            "",
                            sub_thread_id,
                            sub_run_id,
                            "acp",
                            title,
                            1,
                            SubSessionEventPayload::Done {
                                final_output: Some(response.final_summary.clone()),
                            },
                        ));
                    } else if let Some(error) = status.error.clone() {
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            "",
                            sub_thread_id,
                            sub_run_id,
                            "acp",
                            title,
                            1,
                            SubSessionEventPayload::Error { message: error },
                        ));
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
        let title = Some("Codex ACP".to_string());
        if let Some(response) = status.response.clone() {
            yield ToolOutput::SubSession(SubSessionEvent::new(
                "",
                sub_thread_id.clone(),
                sub_run_id.clone(),
                "acp",
                title.clone(),
                1,
                SubSessionEventPayload::Delta {
                    content: response.reply.clone(),
                },
            ));
            yield ToolOutput::SubSession(SubSessionEvent::new(
                "",
                sub_thread_id,
                sub_run_id,
                "acp",
                title,
                1,
                SubSessionEventPayload::Done {
                    final_output: Some(response.final_summary.clone()),
                },
            ));
        } else if let Some(error) = status.error.clone() {
            yield ToolOutput::SubSession(SubSessionEvent::new(
                "",
                sub_thread_id,
                sub_run_id,
                "acp",
                title,
                1,
                SubSessionEventPayload::Error { message: error },
            ));
        }
        yield ToolOutput::text(preview);
    }
    .boxed()
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
    use super::{current_bound_channel, AcpChatTool};
    use crate::acp::backend::AcpBackend;
    use remi_agentloop::prelude::{AgentConfig, Tool, ToolContext};
    use std::sync::{Arc, RwLock};

    fn tool_context(metadata: Option<serde_json::Value>) -> ToolContext {
        ToolContext {
            config: AgentConfig::default(),
            thread_id: Some(serde_json::from_value(serde_json::json!("thread-1")).unwrap()),
            run_id: serde_json::from_value(serde_json::json!("run-1")).unwrap(),
            metadata,
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
        let tool = AcpChatTool::codex(Arc::new(AcpBackend::new(
            tempdir.path().to_path_buf(),
            None,
        )));
        let schema = tool.parameters_schema();

        assert_eq!(tool.name(), "codex");
        assert!(tool.description().contains("session_id"));
        assert!(schema["properties"].get("session_id").is_some());
    }
}
