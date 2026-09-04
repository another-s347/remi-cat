//! Memory tools.
//!
//! The thread_id is forwarded via `ctx.metadata["thread_id"]`, which is set
//! in `CatBot::stream()` via `LoopInput::metadata(json!({"thread_id": ...}))`.
//! The metadata propagates through `AgentLoop` into every `ToolContext`.

use async_stream::stream;
use bot_runtime_core::ToolContext;
use futures::Stream;
use remi_agentloop::prelude::{AgentError, Content, Tool, ToolOutput, ToolResult};
use remi_agentloop::types::ResumePayload;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::latest_summary_message;
use super::store::MemoryStore;
use crate::{ContextCompactionEvent, ContextCompactionSource, ContextCompactionStatus};
use remi_agentloop::prelude::ContextOperation;

pub struct ContextManageTool {
    pub store: Arc<MemoryStore>,
    pub agent_id: String,
}

pub struct MemoryGetDetailTool {
    pub store: Arc<MemoryStore>,
    pub agent_id: String,
}

pub struct MemoryUpsertNamedTool {
    pub store: Arc<MemoryStore>,
    pub agent_id: String,
    pub workspace_root: PathBuf,
}

pub struct MemoryRecallTool {
    pub store: Arc<MemoryStore>,
    pub agent_id: String,
}

impl Tool for ContextManageTool {
    fn name(&self) -> &str {
        "context__manage"
    }

    fn description(&self) -> &str {
        "Manage the conversation context. Use operation=replace_prior when earlier information has \
         become overly complex, stale, redundant, or when the user asks you to organize/compact the \
         context. Write the summary yourself and call this tool alone. Preserve the conversation's \
         primary language; latest user intent, constraints, confirmed facts and decisions; completed \
         work and evidence; current state and pending work; failures, uncertainties, paths, IDs, exact \
         values, commands, and errors. Resolve superseded facts chronologically and omit genuinely \
         obsolete or irrelevant detail. The operation replaces prior conversational messages while \
         retaining standing system instructions and the current run. Replaced messages will be absent \
         from subsequent model requests, and the submitted summary will be their only replacement, so \
         include every detail needed to continue correctly before calling the tool."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["replace_prior"],
                    "description": "Replace prior conversational context with the supplied summary"
                },
                "summary": {
                    "type": "string",
                    "description": "Complete, information-dense summary authored by the agent. This becomes the only replacement for prior mutable conversation messages in subsequent model requests."
                }
            },
            "required": ["operation", "summary"],
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
        let operation = args.get("operation").and_then(serde_json::Value::as_str);
        if operation != Some("replace_prior") {
            return Err(AgentError::tool(
                "context__manage",
                "operation must be replace_prior",
            ));
        }
        let summary = args
            .get("summary")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if summary.is_empty() {
            return Err(AgentError::tool(
                "context__manage",
                "summary must not be empty",
            ));
        }
        let thread_id =
            memory_thread_id_from_args_or_context(&args, ctx, &self.agent_id, "context__manage")?;
        let store = Arc::clone(&self.store);
        let operation_id = uuid::Uuid::new_v4().to_string();

        Ok(ToolResult::Output(stream! {
            let started = ContextCompactionEvent {
                id: operation_id.clone(),
                thread_id: thread_id.clone(),
                status: ContextCompactionStatus::Started,
                source: ContextCompactionSource::Agent,
                compacted_messages: 0,
                remaining_messages: 0,
                error: None,
            };
            yield ToolOutput::custom(
                "remi.context_compaction",
                serde_json::to_value(started).unwrap_or_default(),
            );
            match store.commit_agent_summary(&thread_id, &summary).await {
                Ok(0) => {
                    let error = "no prior persisted conversation is available to replace";
                    let failed = ContextCompactionEvent {
                        id: operation_id,
                        thread_id,
                        status: ContextCompactionStatus::Failed,
                        source: ContextCompactionSource::Agent,
                        compacted_messages: 0,
                        remaining_messages: 0,
                        error: Some(error.to_string()),
                    };
                    yield ToolOutput::custom(
                        "remi.context_compaction",
                        serde_json::to_value(failed).unwrap_or_default(),
                    );
                    yield ToolOutput::text(format!("Error: {error}"));
                }
                Ok(compacted_messages) => {
                    yield ToolOutput::ContextOperation(ContextOperation::ReplacePriorContext {
                        messages: vec![latest_summary_message(&summary)],
                    });
                    let completed = ContextCompactionEvent {
                        id: operation_id,
                        thread_id,
                        status: ContextCompactionStatus::Completed,
                        source: ContextCompactionSource::Agent,
                        compacted_messages,
                        remaining_messages: 0,
                        error: None,
                    };
                    yield ToolOutput::custom(
                        "remi.context_compaction",
                        serde_json::to_value(completed).unwrap_or_default(),
                    );
                    yield ToolOutput::text(format!(
                        "Context replaced with the supplied summary; covered {compacted_messages} persisted message(s)."
                    ));
                }
                Err(err) => {
                    let failed = ContextCompactionEvent {
                        id: operation_id,
                        thread_id,
                        status: ContextCompactionStatus::Failed,
                        source: ContextCompactionSource::Agent,
                        compacted_messages: 0,
                        remaining_messages: 0,
                        error: Some(err.to_string()),
                    };
                    yield ToolOutput::custom(
                        "remi.context_compaction",
                        serde_json::to_value(failed).unwrap_or_default(),
                    );
                    yield ToolOutput::text(format!("Error: {err}"));
                }
            }
        }))
    }
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
                },
                "message_id": {
                    "type": "string",
                    "description": "Optional ledger message ID to retrieve"
                },
                "name": {
                    "type": "string",
                    "description": "Optional named memory file name to retrieve instead of a UUID"
                },
                "agent": {
                    "type": "string",
                    "description": "Optional agent id for named memory or named session memory. Defaults to the current agent."
                },
                "named": {
                    "type": "string",
                    "description": "Optional named persistent sub-agent session whose thread memory should be searched for uuid."
                }
            },
            "required": []
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
        let uuid = args["message_id"]
            .as_str()
            .or_else(|| args["uuid"].as_str())
            .unwrap_or("")
            .to_string();
        let name = args["name"].as_str().unwrap_or("").to_string();
        let thread_id = memory_thread_id_from_args_or_context(
            &args,
            ctx,
            &self.agent_id,
            "memory__get_detail",
        )?;
        let agent_id = memory_agent_from_args(&args, &self.agent_id, "memory__get_detail")?;
        let store = Arc::clone(&self.store);

        Ok(ToolResult::Output(stream! {
            if uuid.trim().is_empty() && name.trim().is_empty() {
                yield ToolOutput::Result(Content::text("Error: uuid or name parameter is required"));
                return;
            }
            if !name.trim().is_empty() {
                match store.get_named_memory(&agent_id, &name).await {
                    Ok(Some(text)) => yield ToolOutput::Result(Content::text(text)),
                    Ok(None) => yield ToolOutput::Result(Content::text(format!(
                        "No named memory found for name: {name}"
                    ))),
                    Err(e) => yield ToolOutput::Result(Content::text(format!(
                        "Error reading named memory: {e}"
                    ))),
                }
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
        _ctx: ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
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
        let workspace_root = self.workspace_root.clone();

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
                    let fs_read_path = workspace_relative_path(&workspace_root, &saved.path);
                    let path = fs_read_path
                        .clone()
                        .unwrap_or_else(|| saved.path.display().to_string());
                    let mut payload = json!({
                        "name": saved.name,
                        "path": path,
                        "absolute_path": saved.path.display().to_string(),
                        "created_at": saved.created_at.to_rfc3339(),
                        "updated_at": saved.updated_at.to_rfc3339(),
                        "bytes": saved.bytes,
                    });
                    if let Some(fs_read_path) = fs_read_path {
                        payload["fs_read_path"] = json!(fs_read_path);
                    } else {
                        payload["fs_read_note"] = json!(
                            "memory file is outside the workspace root and cannot be read with fs_read"
                        );
                    }
                    yield ToolOutput::Result(Content::text(serde_json::to_string_pretty(&payload)
                        .unwrap_or_else(|_| "saved named memory".to_string())));
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
                "message_id": {
                    "type": "string",
                    "description": "Optional exact ledger message ID; use with query for compatibility"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum results to return; defaults to 8 and caps at 20"
                },
                "agent": {
                    "type": "string",
                    "description": "Optional agent id for named session memory. Defaults to the current agent."
                },
                "named": {
                    "type": "string",
                    "description": "Optional named persistent sub-agent session whose thread memory should be searched."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
        let query = args
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let limit = args
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(8) as usize;
        let thread_id =
            memory_thread_id_from_args_or_context(&args, ctx, &self.agent_id, "memory__recall")?;
        let recall_agent_id = memory_agent_from_args(&args, &self.agent_id, "memory__recall")?;
        let store = Arc::clone(&self.store);

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
            match store.recall(&recall_agent_id, &thread_id, &query, limit).await {
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

pub(crate) fn memory_agent_from_args(
    args: &serde_json::Value,
    default_agent_id: &str,
    tool_name: &str,
) -> Result<String, AgentError> {
    let agent = args
        .get("agent")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default_agent_id);
    validate_memory_identifier(tool_name, "agent", agent)?;
    Ok(agent.to_string())
}

pub(crate) fn memory_thread_id_from_args_or_context(
    args: &serde_json::Value,
    ctx: ToolContext,
    default_agent_id: &str,
    tool_name: &str,
) -> Result<String, AgentError> {
    if let Some(named) = args
        .get("named")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        validate_memory_identifier(tool_name, "named", named)?;
        let agent = memory_agent_from_args(args, default_agent_id, tool_name)?;
        return Ok(named_memory_thread_id(&agent, named));
    }

    let metadata = ctx.metadata();
    metadata
        .as_ref()
        .and_then(|m| m.get("thread_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| Some(ctx.thread_id().to_string()))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| AgentError::tool(tool_name, "thread_id not found in context metadata"))
}

pub(crate) fn named_memory_thread_id(agent_id: &str, named: &str) -> String {
    format!("subagent:{agent_id}:{named}")
}

fn validate_memory_identifier(tool_name: &str, field: &str, value: &str) -> Result<(), AgentError> {
    if value.len() > 64 {
        return Err(AgentError::tool(
            tool_name,
            format!("{field} must be at most 64 characters"),
        ));
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(AgentError::tool(
            tool_name,
            format!("{field} may only contain ASCII letters, numbers, '-' and '_'"),
        ));
    }
    Ok(())
}

fn workspace_relative_path(workspace_root: &Path, path: &Path) -> Option<String> {
    let workspace_root = absolute_lexical_path(workspace_root);
    let path = absolute_lexical_path(path);
    let relative = path.strip_prefix(workspace_root).ok()?;
    if relative.as_os_str().is_empty() {
        Some(".".to_string())
    } else {
        Some(relative.to_string_lossy().replace('\\', "/"))
    }
}

fn absolute_lexical_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use remi_agentloop::prelude::{Message, Tool};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn test_store(data_dir: PathBuf) -> Arc<MemoryStore> {
        Arc::new(MemoryStore {
            memory_dir: data_dir.join("memory"),
            data_dir,
            agent_md_path: None,
            compressor: super::super::LlmCompressor::new(
                "test-key".to_string(),
                None,
                "gpt-4o-mini".to_string(),
                128_000,
                4096,
                serde_json::Map::new(),
            ),
            short_term_tokens: 8192,
            auto_compress: false,
            memory_days: 7,
        })
    }

    fn tool_context(thread_id: Option<&str>) -> ToolContext {
        ToolContext::with_ids(
            serde_json::from_value(serde_json::json!("test-thread"))
                .expect("thread_id should deserialize"),
            serde_json::from_value(serde_json::json!("test-run"))
                .expect("run_id should deserialize"),
            bot_runtime_core::ChatCtxState {
                metadata: thread_id.map(|id| json!({ "thread_id": id })),
                ..bot_runtime_core::ChatCtxState::default()
            },
        )
    }

    async fn collect_text(result: ToolResult<impl Stream<Item = ToolOutput> + 'static>) -> String {
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
    async fn context_manage_commits_summary_without_deleting_ledger_and_emits_operation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        store
            .save_turn(
                "thread-1",
                vec![
                    Message::user("old request"),
                    Message::assistant("old answer"),
                ],
            )
            .await
            .unwrap();
        let tool = ContextManageTool {
            store: Arc::clone(&store),
            agent_id: "default".to_string(),
        };
        let result = <ContextManageTool as Tool>::execute(
            &tool,
            json!({
                "operation": "replace_prior",
                "summary": "Keep the confirmed decision and pending task."
            }),
            None,
            tool_context(Some("thread-1")),
        )
        .await
        .unwrap();

        let ToolResult::Output(output) = result else {
            panic!("context management must complete without interrupting");
        };
        let outputs = output.collect::<Vec<_>>().await;
        let operations = outputs
            .iter()
            .filter_map(|output| match output {
                ToolOutput::ContextOperation(operation) => Some(operation),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(operations.len(), 1);
        let ContextOperation::ReplacePriorContext { messages } = operations[0];
        assert_eq!(messages.len(), 1);
        assert!(messages[0]
            .content
            .text_content()
            .contains("Keep the confirmed decision and pending task."));

        let context = store.load_context("thread-1").await.unwrap();
        assert!(context
            .latest_summary
            .as_deref()
            .unwrap_or_default()
            .contains("Keep the confirmed decision and pending task."));
        let ledger = tokio::fs::read_to_string(
            tmp.path()
                .join("memory")
                .join("thread-1")
                .join("short_term.jsonl"),
        )
        .await
        .unwrap();
        assert_eq!(ledger.lines().count(), 2);
    }

    #[tokio::test]
    async fn upsert_named_tool_reports_missing_parameters() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = MemoryUpsertNamedTool {
            store: test_store(tmp.path().to_path_buf()),
            agent_id: "default".to_string(),
            workspace_root: tmp.path().to_path_buf(),
        };

        let text = collect_text(
            <MemoryUpsertNamedTool as Tool>::execute(
                &tool,
                json!({ "content": "x" }),
                None,
                tool_context(None),
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
                tool_context(None),
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
                tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(text.contains("query parameter is required"));
    }

    #[tokio::test]
    async fn tools_use_injected_agent_id_and_thread_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        let upsert = MemoryUpsertNamedTool {
            store: Arc::clone(&store),
            agent_id: "planner".to_string(),
            workspace_root: tmp.path().to_path_buf(),
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
                tool_context(Some("thread-1")),
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
                tool_context(Some("thread-1")),
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
                tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        assert_eq!(coder.trim(), "[]");
    }

    #[tokio::test]
    async fn recall_tool_can_target_named_subagent_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        store
            .save_turn(
                "subagent:coder:feature_a",
                vec![Message::user("named session contains alpha")],
            )
            .await
            .unwrap();
        store
            .save_turn(
                "thread-1",
                vec![Message::user("ordinary thread contains beta")],
            )
            .await
            .unwrap();

        let tool = MemoryRecallTool {
            store,
            agent_id: "default".to_string(),
        };

        let text = collect_text(
            <MemoryRecallTool as Tool>::execute(
                &tool,
                json!({ "query": "alpha", "agent": "coder", "named": "feature_a" }),
                None,
                tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(text.contains("alpha"));
        assert!(!text.contains("beta"));
    }

    #[tokio::test]
    async fn get_detail_tool_reads_named_memory_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = test_store(tmp.path().to_path_buf());
        store
            .upsert_named_memory("coder", "project", "alpha named memory")
            .await
            .unwrap();
        let tool = MemoryGetDetailTool {
            store,
            agent_id: "default".to_string(),
        };

        let text = collect_text(
            <MemoryGetDetailTool as Tool>::execute(
                &tool,
                json!({ "name": "project", "agent": "coder" }),
                None,
                tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        assert!(text.contains("alpha named memory"));
    }

    #[tokio::test]
    async fn upsert_named_tool_returns_fs_read_path_relative_to_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join(".remi-cat");
        let tool = MemoryUpsertNamedTool {
            store: test_store(data_dir),
            agent_id: "default".to_string(),
            workspace_root: tmp.path().to_path_buf(),
        };

        let text = collect_text(
            <MemoryUpsertNamedTool as Tool>::execute(
                &tool,
                json!({ "name": "project", "content": "alpha" }),
                None,
                tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        let payload: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            payload["fs_read_path"],
            ".remi-cat/memory/named/default/project.md"
        );
        assert_eq!(payload["path"], ".remi-cat/memory/named/default/project.md");
    }

    #[tokio::test]
    async fn upsert_named_tool_avoids_double_data_dir_when_workspace_is_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = MemoryUpsertNamedTool {
            store: test_store(tmp.path().to_path_buf()),
            agent_id: "default".to_string(),
            workspace_root: tmp.path().to_path_buf(),
        };

        let text = collect_text(
            <MemoryUpsertNamedTool as Tool>::execute(
                &tool,
                json!({ "name": "project", "content": "alpha" }),
                None,
                tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        let payload: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(payload["fs_read_path"], "memory/named/default/project.md");
        assert_eq!(payload["path"], "memory/named/default/project.md");
    }
}
