//! Core drive loop for `CatBot`.
//!
//! `run()` wraps any `AgentLoop` (or composable inner agent), registers
//! skill and todo tools locally, drives the inner agent, handles
//! `NeedToolExecution` by executing local tools and resuming, and finally
//! emits `CatEvent::History` + `CatEvent::Done` when the run finishes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use async_stream::stream;
use futures::{Stream, StreamExt};
use tracing::debug;

use remi_agentloop::prelude::{
    AgentConfig, AgentError, Content, LoopInput, Message, ParsedToolCall, ToolCallOutcome,
    ToolContext, ToolOutput, ToolResult,
};
use remi_agentloop::tool::registry::{DefaultToolRegistry, ToolRegistry};
use remi_agentloop::types::AgentEvent;

use crate::events::CatEvent;
use crate::im_tools::{register_im_tools, ImFileBridge};
use crate::skill;
use crate::todo;

// ── CatAgent ─────────────────────────────────────────────────────────────────

/// Drives an inner agent loop, handling skill and todo tools locally.
pub struct CatAgent<I> {
    /// The inner agent (AgentLoop or composed layers).
    pub inner: I,
    /// Locally-handled tools (skill__* + todo__*).
    pub local_tools: DefaultToolRegistry,
    /// Workspace root — oversized tool outputs are spilled here.
    pub data_dir: PathBuf,
    /// Tool-output byte threshold above which output is spilled to a temp file.
    /// Configured from the model profile; can be overridden per-builder.
    pub overflow_bytes: usize,
    /// Optional daemon-mediated IM bridge used for per-platform upload/download tools.
    pub im_bridge: Option<Arc<dyn ImFileBridge>>,
}

impl<I> CatAgent<I>
where
    I: remi_agentloop::prelude::Agent<
        Request = LoopInput,
        Response = AgentEvent,
        Error = AgentError,
    >,
{
    /// Drive the agent from a pre-built `LoopInput`.
    ///
    /// Called by `CatBot::stream()` in lib.rs after injecting memory context.
    pub fn stream_with_input<'a>(&'a self, input: LoopInput) -> impl Stream<Item = CatEvent> + 'a {
        let data_dir = self.data_dir.clone();
        let overflow_bytes = self.overflow_bytes;
        let dynamic_tools = build_dynamic_tools(input_metadata(&input), data_dir.clone(), self.im_bridge.clone());
        let mut extra_defs = self.local_tools.definitions(&serde_json::Value::Null);
        extra_defs.extend(dynamic_tools.definitions(&serde_json::Value::Null));

        stream! {
            let run_start = Instant::now();
            let mut total_prompt_tokens: u32 = 0;
            let mut total_completion_tokens: u32 = 0;

            let mut current = inject_extra_tools(input, extra_defs);
            let mut last_messages: Vec<Message> = vec![];
            let mut last_user_state: serde_json::Value = serde_json::Value::Null;

            loop {
                let inner_stream = match self.inner.chat(current).await {
                    Ok(s) => s,
                    Err(e) => { yield CatEvent::Error(e); return; }
                };
                let mut inner_stream = std::pin::pin!(inner_stream);
                let mut next_input: Option<LoopInput> = None;

                while let Some(ev) = inner_stream.next().await {
                    match ev {
                        // ── Text deltas pass through ─────────────────────────
                        AgentEvent::TextDelta(t) => yield CatEvent::Text(t),

                        // ── Thinking / reasoning ─────────────────────────────
                        AgentEvent::ThinkingEnd { content } => {
                            yield CatEvent::Thinking(content);
                        }

                        // ── Token usage ──────────────────────────────────────
                        AgentEvent::Usage { prompt_tokens, completion_tokens } => {
                            total_prompt_tokens += prompt_tokens;
                            total_completion_tokens += completion_tokens;
                        }

                        // ── Tool execution ───────────────────────────────────
                        AgentEvent::NeedToolExecution { mut state, tool_calls, completed_results } => {
                            let (local, remaining): (Vec<_>, Vec<_>) = tool_calls
                                .iter()
                                .cloned()
                                .partition(|tc| self.local_tools.contains(&tc.name));
                            let (dynamic, external): (Vec<_>, Vec<_>) = remaining
                                .into_iter()
                                .partition(|tc| dynamic_tools.contains(&tc.name));

                            if !local.is_empty() {
                                let names: Vec<&str> = local.iter().map(|t| t.name.as_str()).collect();
                                debug!(tools = ?names, "agent: dispatching local tools");
                            }
                            if !dynamic.is_empty() {
                                let names: Vec<&str> = dynamic.iter().map(|t| t.name.as_str()).collect();
                                debug!(tools = ?names, "agent: dispatching dynamic tools");
                            }

                            let tool_ctx = build_tool_ctx(&state);
                            let mut all_outcomes: Vec<ToolCallOutcome> = completed_results;

                            if !local.is_empty() {
                                // Emit ToolCall events before execution
                                for tc in &local {
                                    yield CatEvent::ToolCall {
                                        name: tc.name.clone(),
                                        args: tc.arguments.clone(),
                                    };
                                }

                                let resume_map = HashMap::new();
                                let results = self.local_tools
                                    .execute_parallel(&local, &resume_map, &tool_ctx)
                                    .await;

                                for (call_id, result) in results {
                                    let tc = local.iter().find(|t| t.id == call_id).unwrap();
                                    debug!(tool = %tc.name, "agent: collecting tool result");
                                    let result_str = collect_result_with_overflow(result, &data_dir, overflow_bytes).await;
                                    debug!(tool = %tc.name, len = result_str.len(), "agent: tool done");

                                    // Emit ToolCallResult
                                    yield CatEvent::ToolCallResult {
                                        name: tc.name.clone(),
                                        result: result_str.clone(),
                                    };

                                    // Emit side events for mutations
                                    for side_ev in make_side_events(tc, &result_str) {
                                        yield side_ev;
                                    }

                                    all_outcomes.push(ToolCallOutcome::Result {
                                        tool_call_id: call_id,
                                        tool_name: tc.name.clone(),
                                        content: Content::text(result_str),
                                    });
                                }

                                // Write back user_state changes (todo tools)
                                state.user_state =
                                    tool_ctx.user_state.read().unwrap().clone();

                                // Notify lib.rs to persist user_state eagerly
                                yield CatEvent::StateUpdate(state.user_state.clone());
                            }

                            if !dynamic.is_empty() {
                                for tc in &dynamic {
                                    yield CatEvent::ToolCall {
                                        name: tc.name.clone(),
                                        args: tc.arguments.clone(),
                                    };
                                }

                                let resume_map = HashMap::new();
                                let results = dynamic_tools
                                    .execute_parallel(&dynamic, &resume_map, &tool_ctx)
                                    .await;

                                for (call_id, result) in results {
                                    let tc = dynamic.iter().find(|t| t.id == call_id).unwrap();
                                    debug!(tool = %tc.name, "agent: collecting dynamic tool result");
                                    let result_str = collect_result_with_overflow(result, &data_dir, overflow_bytes).await;
                                    debug!(tool = %tc.name, len = result_str.len(), "agent: dynamic tool done");

                                    yield CatEvent::ToolCallResult {
                                        name: tc.name.clone(),
                                        result: result_str.clone(),
                                    };

                                    all_outcomes.push(ToolCallOutcome::Result {
                                        tool_call_id: call_id,
                                        tool_name: tc.name.clone(),
                                        content: Content::text(result_str),
                                    });
                                }
                            }

                            if !external.is_empty() {
                                // We don't handle external tools — surface the error.
                                yield CatEvent::Error(AgentError::tool(
                                    "external",
                                    format!("unhandled external tools: {}", external.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", ")),
                                ));
                                return;
                            }

                            next_input = Some(LoopInput::Resume { state, results: all_outcomes });
                            break;
                        }

                        // ── Capture messages from checkpoints ────────────────
                        AgentEvent::Checkpoint(cp) => {
                            last_messages = cp.state.messages.clone();
                            last_user_state = cp.state.user_state.clone();
                        }

                        // ── Terminal events ──────────────────────────────────
                        AgentEvent::Done => {
                            let elapsed_ms = run_start.elapsed().as_millis() as u64;
                            yield CatEvent::Stats {
                                prompt_tokens: total_prompt_tokens,
                                completion_tokens: total_completion_tokens,
                                elapsed_ms,
                            };
                            yield CatEvent::History(last_messages.clone(), last_user_state.clone());
                            yield CatEvent::Done;
                            return;
                        }
                        AgentEvent::Error(e) => {
                            yield CatEvent::Error(e);
                            return;
                        }
                        AgentEvent::Cancelled => {
                            yield CatEvent::Done;
                            return;
                        }

                        // ── Ignore other events (ToolDelta, ToolResult, RunStart, …) ─
                        _ => {}
                    }
                }

                match next_input {
                    Some(n) => current = n,
                    None => return,
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn inject_extra_tools(
    input: LoopInput,
    extra: Vec<remi_agentloop::prelude::ToolDefinition>,
) -> LoopInput {
    match input {
        LoopInput::Start {
            content,
            history,
            mut extra_tools,
            model,
            temperature,
            max_tokens,
            user_name,
            metadata,
            message_metadata,
            user_state,
        } => {
            extra_tools.extend(extra);
            LoopInput::Start {
                content,
                history,
                extra_tools,
                model,
                temperature,
                max_tokens,
                user_name,
                metadata,
                message_metadata,
                user_state,
            }
        }
        other => other,
    }
}

fn build_tool_ctx(state: &remi_agentloop::prelude::AgentState) -> ToolContext {
    ToolContext {
        config: AgentConfig::default(),
        thread_id: Some(state.thread_id.clone()),
        run_id: state.run_id.clone(),
        metadata: state.config.metadata.clone(),
        user_state: Arc::new(RwLock::new(state.user_state.clone())),
    }
}

fn input_metadata(input: &LoopInput) -> Option<serde_json::Value> {
    match input {
        LoopInput::Start { metadata, .. } => metadata.clone(),
        LoopInput::Resume { state, .. } => state.config.metadata.clone(),
        LoopInput::Cancel { state } => state.config.metadata.clone(),
    }
}

fn build_dynamic_tools(
    metadata: Option<serde_json::Value>,
    data_dir: PathBuf,
    im_bridge: Option<Arc<dyn ImFileBridge>>,
) -> DefaultToolRegistry {
    let mut registry = DefaultToolRegistry::new();
    let Some(bridge) = im_bridge else {
        return registry;
    };
    let platform = metadata
        .as_ref()
        .and_then(|value| value.get("platform"))
        .and_then(|value| value.as_str());
    if platform == Some("feishu") {
        register_im_tools(&mut registry, data_dir, bridge);
    }
    registry
}

/// Maximum byte length of a tool result returned inline.
/// Larger output is spilled to a temp file in `<data_dir>/tmp/`.
/// This const is the hard floor; the actual threshold comes from
/// [`CatAgent::overflow_bytes`] which is derived from the model profile.
const _OVERFLOW_THRESHOLD_DEFAULT: usize = 20_000;

async fn collect_result(
    result: Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>,
) -> String {
    match result {
        Err(e) => format!("error: {e}"),
        Ok(ToolResult::Interrupt(_)) => "interrupted".to_string(),
        Ok(ToolResult::Output(s)) => {
            let mut s = std::pin::pin!(s);
            let mut last = String::new();
            while let Some(out) = s.next().await {
                if let ToolOutput::Result(c) = out {
                    last = c.text_content();
                }
            }
            last
        }
    }
}

async fn collect_result_with_overflow(
    result: Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>,
    data_dir: &Path,
    overflow_bytes: usize,
) -> String {
    let text = collect_result(result).await;
    if text.len() <= overflow_bytes {
        return text;
    }
    // Spill to tmp file so the agent can read it in chunks via fs_read.
    let tmp_dir = data_dir.join("tmp");
    let _ = tokio::fs::create_dir_all(&tmp_dir).await;
    let filename = format!("tool_out_{}.txt", uuid::Uuid::new_v4());
    let file_path = tmp_dir.join(&filename);
    let total = text.len();
    match tokio::fs::write(&file_path, text.as_bytes()).await {
        Ok(()) => format!(
            "[Output too large ({total} bytes) — saved to tmp/{filename}]\n\
             Use fs_read with path=\"tmp/{filename}\" (offset=0, length=8192) \
             and increment offset until remaining=0."
        ),
        Err(_) => text, // fall back to inline if write fails
    }
}

fn make_side_events(tc: &ParsedToolCall, result_str: &str) -> Vec<CatEvent> {
    let mut evs = Vec::new();
    if let Some(ev) = skill::make_skill_event(tc) {
        evs.push(CatEvent::Skill(ev));
    }
    if let Some(ev) = todo::make_todo_event(tc, result_str) {
        evs.push(CatEvent::Todo(ev));
    }
    evs
}
