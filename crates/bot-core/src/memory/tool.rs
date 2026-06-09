//! Memory tools.
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

pub struct MemoryUpsertNamedTool {
    pub store: Arc<MemoryStore>,
    pub agent_id: String,
}

pub struct MemoryRecallTool {
    pub store: Arc<MemoryStore>,
    pub agent_id: String,
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

impl Tool for MemoryUpsertNamedTool {
    fn name(&self) -> &str {
        "memory__upsert_named"
    }

    fn description(&self) -> &str {
        "Create or replace one named long-lived memory for the current agent. \
         Use this for stable facts, user preferences, project conventions, or decisions \
         that should be remembered later without occupying every turn's context."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Simple memory file name chosen by the agent; .md is added if omitted"
                },
                "content": {
                    "type": "string",
                    "description": "Complete markdown content to store for this named memory"
                }
            },
            "required": ["name", "content"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let name = args
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let content = args
            .get("content")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let store = Arc::clone(&self.store);
        let agent_id = self.agent_id.clone();

        Ok(ToolResult::Output(stream! {
            if name.trim().is_empty() {
                yield ToolOutput::Result(Content::text("Error: name parameter is required"));
                return;
            }
            if content.trim().is_empty() {
                yield ToolOutput::Result(Content::text("Error: content parameter is required"));
                return;
            }
            match store.upsert_named_memory(&agent_id, &name, &content).await {
                Ok(saved) => {
                    yield ToolOutput::Result(Content::text(serde_json::to_string_pretty(&json!({
                        "name": saved.name,
                        "path": saved.path.display().to_string(),
                        "created_at": saved.created_at.to_rfc3339(),
                        "updated_at": saved.updated_at.to_rfc3339(),
                        "bytes": saved.bytes,
                    })).unwrap_or_else(|_| "saved named memory".to_string())));
                }
                Err(err) => {
                    yield ToolOutput::Result(Content::text(format!("Error saving named memory: {err}")));
                }
            }
        }))
    }
}

impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory__recall"
    }

    fn description(&self) -> &str {
        "Search memory when you need to recall prior facts. Searches the current agent's \
         named memories plus this thread's short-term, mid-term, and long-term memory. \
         Use a few distinctive keywords and inspect timestamps in the results."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keyword query to search for"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return; defaults to 8 and caps at 20"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let query = args
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let limit = args
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(8) as usize;
        let thread_id = ctx
            .metadata
            .as_ref()
            .and_then(|m| m.get("thread_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let store = Arc::clone(&self.store);
        let agent_id = self.agent_id.clone();

        Ok(ToolResult::Output(stream! {
            if query.trim().is_empty() {
                yield ToolOutput::Result(Content::text("Error: query parameter is required"));
                return;
            }
            if thread_id.is_empty() {
                yield ToolOutput::Result(Content::text(
                    "Error: thread_id not found in context metadata",
                ));
                return;
            }
            match store.recall(&agent_id, &thread_id, &query, limit).await {
                Ok(results) => {
                    let rows: Vec<_> = results
                        .into_iter()
                        .map(|result| json!({
                            "source": result.source,
                            "name": result.name,
                            "uuid": result.uuid,
                            "timestamp": result.timestamp.to_rfc3339(),
                            "preview": result.preview,
                            "snippet": result.snippet,
                            "score": result.score,
                        }))
                        .collect();
                    yield ToolOutput::Result(Content::text(serde_json::to_string_pretty(&rows)
                        .unwrap_or_else(|_| "[]".to_string())));
                }
                Err(err) => {
                    yield ToolOutput::Result(Content::text(format!("Error recalling memory: {err}")));
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use remi_agentloop::prelude::{AgentConfig, Tool};
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    fn test_store(data_dir: PathBuf) -> Arc<MemoryStore> {
        Arc::new(MemoryStore {
            data_dir,
            agent_md_path: None,
            compressor: super::super::LlmCompressor::new(
                "test-key".to_string(),
                None,
                "gpt-4o-mini".to_string(),
                4096,
                serde_json::Map::new(),
            ),
            short_term_tokens: 8192,
            auto_compress: false,
            memory_days: 7,
        })
    }

    fn tool_context(thread_id: Option<&str>) -> ToolContext {
        ToolContext {
            config: AgentConfig::default(),
            thread_id: Some(
                serde_json::from_value(serde_json::json!("test-thread"))
                    .expect("thread_id should deserialize"),
            ),
            run_id: serde_json::from_value(serde_json::json!("test-run"))
                .expect("run_id should deserialize"),
            metadata: thread_id.map(|id| json!({ "thread_id": id })),
            user_state: Arc::new(RwLock::new(serde_json::Value::Null)),
        }
    }

    async fn collect_text(result: ToolResult<impl Stream<Item = ToolOutput>>) -> String {
        match result {
            ToolResult::Interrupt(_) => "interrupted".to_string(),
            ToolResult::Output(output) => {
                let mut output = std::pin::pin!(output);
                let mut text = String::new();
                while let Some(item) = output.next().await {
                    if let ToolOutput::Result(content) = item {
                        text = content.text_content();
                    }
                }
                text
            }
        }
    }

    #[tokio::test]
    async fn upsert_named_tool_reports_missing_parameters() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = MemoryUpsertNamedTool {
            store: test_store(tmp.path().to_path_buf()),
            agent_id: "default".to_string(),
        };

        let text = collect_text(
            <MemoryUpsertNamedTool as Tool>::execute(
                &tool,
                json!({ "content": "x" }),
                None,
                &tool_context(None),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(text.contains("name parameter is required"));

        let text = collect_text(
            <MemoryUpsertNamedTool as Tool>::execute(
                &tool,
                json!({ "name": "x" }),
                None,
                &tool_context(None),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(text.contains("content parameter is required"));
    }

    #[tokio::test]
    async fn recall_tool_reports_missing_query_and_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = MemoryRecallTool {
            store: test_store(tmp.path().to_path_buf()),
            agent_id: "default".to_string(),
        };

        let text = collect_text(
            <MemoryRecallTool as Tool>::execute(
                &tool,
                json!({}),
                None,
                &tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(text.contains("query parameter is required"));

        let text = collect_text(
            <MemoryRecallTool as Tool>::execute(
                &tool,
                json!({ "query": "alpha" }),
                None,
                &tool_context(None),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(text.contains("thread_id not found"));
    }

    #[tokio::test]
    async fn tools_use_injected_agent_id_and_thread_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        let upsert = MemoryUpsertNamedTool {
            store: Arc::clone(&store),
            agent_id: "planner".to_string(),
        };
        let planner_recall = MemoryRecallTool {
            store: Arc::clone(&store),
            agent_id: "planner".to_string(),
        };
        let coder_recall = MemoryRecallTool {
            store,
            agent_id: "coder".to_string(),
        };

        let saved = collect_text(
            <MemoryUpsertNamedTool as Tool>::execute(
                &upsert,
                json!({ "name": "project", "content": "alpha belongs to planner" }),
                None,
                &tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(saved.contains("project.md"));

        let planner = collect_text(
            <MemoryRecallTool as Tool>::execute(
                &planner_recall,
                json!({ "query": "alpha" }),
                None,
                &tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(planner.contains("project.md"));

        let coder = collect_text(
            <MemoryRecallTool as Tool>::execute(
                &coder_recall,
                json!({ "query": "alpha" }),
                None,
                &tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(coder.trim(), "[]");
    }
}
