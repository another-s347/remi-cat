use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{AgentError, Tool, ToolContext, ToolOutput, ToolResult};
use remi_agentloop::types::{
    ResumePayload, RunId, SubSessionEvent, SubSessionEventPayload, ThreadId,
};
use serde::Deserialize;
use uuid::Uuid;

use super::backend::{AcpBackend, AcpBoundChannel, AcpToolRequest};

pub struct AcpChatTool {
    backend: Arc<AcpBackend>,
}

impl AcpChatTool {
    pub fn new(backend: Arc<AcpBackend>) -> Self {
        Self { backend }
    }
}

#[derive(Debug, Deserialize)]
struct AcpChatArguments {
    message: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

impl Tool for AcpChatTool {
    fn name(&self) -> &str {
        "acp__chat"
    }

    fn description(&self) -> &str {
        "Open or resume an ACP session, automatically bind a new session to the current IM channel, and return the final ACP answer summary."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The user message to send to ACP." },
                "session_id": { "type": "string", "description": "Optional ACP session id to resume." },
                "title": { "type": "string", "description": "Optional title for a newly created ACP session." }
            },
            "required": ["message"]
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
        let ctx = ctx.clone();
        async move {
            let args: AcpChatArguments = serde_json::from_value(arguments).map_err(|_| {
                AgentError::tool("acp__chat", "expected {message, session_id?, title?}")
            })?;
            let current_channel = current_bound_channel(&ctx)?;
            let prepared = backend
                .prepare_tool_turn(AcpToolRequest {
                    message: args.message,
                    session_id: args.session_id,
                    title: args.title,
                    current_channel: Some(current_channel),
                })
                .await
                .map_err(|err| AgentError::tool("acp__chat", err.to_string()))?;

            let sub_thread_id = ThreadId(prepared.sub_session_id.clone());
            let sub_run_id = RunId(Uuid::new_v4().to_string());
            let title = prepared.title.clone();
            let response = backend
                .run_prepared_tool_turn(prepared.clone())
                .await
                .map_err(|err| AgentError::tool("acp__chat", err.to_string()))?;
            let preview = serde_json::to_string_pretty(&response)
                .map_err(|err| AgentError::tool("acp__chat", err.to_string()))?;

            Ok(ToolResult::Output(stream! {
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
                yield ToolOutput::text(preview);
            }))
        }
    }
}

fn current_bound_channel(ctx: &ToolContext) -> Result<AcpBoundChannel, AgentError> {
    let platform = ctx
        .metadata
        .as_ref()
        .and_then(|value| value.get("platform"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::tool("acp__chat", "missing platform in tool context"))?;
    let channel_id = ctx
        .thread_id
        .as_ref()
        .map(|value| value.0.trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AgentError::tool("acp__chat", "missing thread/channel id in tool context")
        })?;
    Ok(AcpBoundChannel {
        platform: platform.to_string(),
        channel_id: channel_id.to_string(),
    })
}
