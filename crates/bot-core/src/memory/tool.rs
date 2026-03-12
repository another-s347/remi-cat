//! `memory__get_detail` tool — retrieves a full memory block by UUID.
//!
//! The thread_id is forwarded via `ctx.metadata["thread_id"]`, which is set
//! in `CatBot::stream()` via `LoopInput::metadata(json!({"thread_id": ...}))`.
//! The metadata propagates through `AgentLoop` into every `ToolContext`.

use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{AgentError, Content, Tool, ToolContext, ToolOutput, ToolResult};
use remi_agentloop::types::ResumePayload;
use serde_json::json;
use std::sync::Arc;

use super::store::MemoryStore;

pub struct MemoryGetDetailTool {
    pub store: Arc<MemoryStore>,
}

impl Tool for MemoryGetDetailTool {
    fn name(&self) -> &str {
        "memory__get_detail"
    }

    fn description(&self) -> &str {
        "Retrieve the full content of a long-term or mid-term memory block by its UUID. \
         Use this when you see a memory entry listed in the context header and want to \
         read the complete compressed summary."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "uuid": {
                    "type": "string",
                    "description": "The UUID of the memory block to retrieve"
                }
            },
            "required": ["uuid"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let uuid = args["uuid"].as_str().unwrap_or("").to_string();
        let thread_id = ctx
            .metadata
            .as_ref()
            .and_then(|m| m.get("thread_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let store = Arc::clone(&self.store);

        Ok(ToolResult::Output(stream! {
            if uuid.is_empty() {
                yield ToolOutput::Result(Content::text("Error: uuid parameter is required"));
                return;
            }
            if thread_id.is_empty() {
                yield ToolOutput::Result(Content::text(
                    "Error: thread_id not found in context metadata",
                ));
                return;
            }
            match store.get_detail(&thread_id, &uuid).await {
                Ok(Some(text)) => yield ToolOutput::Result(Content::text(text)),
                Ok(None) => yield ToolOutput::Result(Content::text(format!(
                    "No memory block found for uuid: {uuid}"
                ))),
                Err(e) => yield ToolOutput::Result(Content::text(format!(
                    "Error reading memory block: {e}"
                ))),
            }
        }))
    }
}
