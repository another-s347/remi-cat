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
use futures::{future::join_all, Stream, StreamExt};
use tracing::debug;

use remi_agentloop::prelude::{
    AgentConfig, AgentError, Content, ContentPart, LoopInput, Message, ParsedToolCall,
    ToolCallOutcome, ToolContext, ToolDefinition, ToolDefinitionContext, ToolOutput, ToolResult,
};
use remi_agentloop::tool::registry::{DefaultToolRegistry, ToolRegistry};
use remi_agentloop::tool::BoxedToolResult;
use remi_agentloop::types::AgentEvent;
use tokio::sync::mpsc;

use crate::approval::{
    classify_tool_risk, summarize_tool_args, ApprovalEvent, ApprovalResolution, ApprovalWait,
    ToolApprovalManager, ToolApprovalRequest,
};
use crate::events::CatEvent;
use crate::im_tools::{register_im_tools, ImFileBridge};
use crate::skill;
use crate::todo;
use crate::trigger;

const TRIGGER_MANAGEMENT_TOOL_NAMES: &[&str] =
    &["trigger__upsert", "trigger__list", "trigger__delete"];
const SUPERVISOR_MAX_TOOL_ROUNDS: usize = 8;

fn log_tool_call_started(kind: &str, thread_id: &str, run_id: &str, tc: &ParsedToolCall) {
    tracing::info!(
        thread_id,
        run_id,
        tool_kind = kind,
        tool_call_id = %tc.id,
        tool_name = %tc.name,
        args_len = tc.arguments.to_string().len(),
        "tool_call.start"
    );
}

fn log_tool_call_finished(
    kind: &str,
    thread_id: &str,
    run_id: &str,
    tc: &ParsedToolCall,
    collected: &CollectedToolResult,
    success: bool,
    elapsed_ms: u64,
) {
    if success {
        tracing::info!(
            thread_id,
            run_id,
            tool_kind = kind,
            tool_call_id = %tc.id,
            tool_name = %tc.name,
            success,
            elapsed_ms,
            preview_len = collected.preview.len(),
            multimodal = collected.content.is_multimodal(),
            side_events = collected.side_events.len(),
            "tool_call.completed"
        );
    } else {
        tracing::warn!(
            thread_id,
            run_id,
            tool_kind = kind,
            tool_call_id = %tc.id,
            tool_name = %tc.name,
            success,
            elapsed_ms,
            preview_len = collected.preview.len(),
            multimodal = collected.content.is_multimodal(),
            side_events = collected.side_events.len(),
            "tool_call.failed"
        );
    }
}

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
    /// Optional explicit tool allowlist from the active agent profile.
    pub tool_allowlist: Option<Vec<String>>,
    /// Shared in-memory approval manager for risky tool calls.
    pub approval_manager: Arc<ToolApprovalManager>,
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
        let tool_allowlist = self.tool_allowlist.clone();
        let tool_def_ctx = build_tool_definition_ctx(&input);
        let supervisor_run =
            crate::metadata_flag_enabled(tool_def_ctx.metadata.as_ref(), "supervisor_run");
        let log_thread_id = tool_def_ctx
            .thread_id
            .as_ref()
            .map(|value| value.0.clone())
            .unwrap_or_default();
        let log_run_id = tool_def_ctx
            .run_id
            .as_ref()
            .map(|value| value.0.clone())
            .unwrap_or_default();
        let dynamic_tools = build_dynamic_tools(
            tool_def_ctx.metadata.clone(),
            data_dir.clone(),
            self.im_bridge.clone(),
        );
        let dynamic_tool_count = dynamic_tools.definitions_with_context(&tool_def_ctx).len();
        let extra_defs = merge_runtime_tool_definitions(
            &self.local_tools,
            &dynamic_tools,
            &tool_def_ctx,
            &[],
            tool_allowlist.as_deref(),
        );

        stream! {
            let run_start = Instant::now();
            tracing::info!(
                thread_id = %log_thread_id,
                run_id = %log_run_id,
                supervisor_run,
                dynamic_tools = dynamic_tool_count,
                "agent_run.start"
            );
            let mut total_prompt_tokens: u32 = 0;
            let mut total_completion_tokens: u32 = 0;
            let mut max_prompt_tokens: u32 = 0;
            let mut tool_rounds: usize = 0;

            let mut current = inject_extra_tools(input, extra_defs);
            let mut last_messages: Vec<Message> = vec![];
            let mut last_user_state: serde_json::Value = serde_json::Value::Null;

            loop {
                let inner_stream = match self.inner.chat(current).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            thread_id = %log_thread_id,
                            run_id = %log_run_id,
                            supervisor_run,
                            elapsed_ms = run_start.elapsed().as_millis() as u64,
                            error = %e,
                            "agent_run.failed"
                        );
                        yield CatEvent::Error(e);
                        return;
                    }
                };
                let mut inner_stream = std::pin::pin!(inner_stream);
                let mut next_input: Option<LoopInput> = None;

                while let Some(ev) = inner_stream.next().await {
                    match ev {
                        AgentEvent::TextDelta(t) => yield CatEvent::Text(t),
                        AgentEvent::ThinkingEnd { content } => {
                            yield CatEvent::Thinking(content);
                        }
                        AgentEvent::ToolCallStart { id, name } => {
                            yield CatEvent::ToolCallStart { id, name };
                        }
                        AgentEvent::ToolCallArgumentsDelta { id, delta } => {
                            yield CatEvent::ToolCallArgumentsDelta { id, delta };
                        }
                        AgentEvent::Usage {
                            prompt_tokens,
                            completion_tokens,
                        } => {
                            total_prompt_tokens += prompt_tokens;
                            total_completion_tokens += completion_tokens;
                            max_prompt_tokens = max_prompt_tokens.max(prompt_tokens);
                        }
                        AgentEvent::NeedToolExecution {
                            mut state,
                            tool_calls,
                            completed_results,
                        } => {
                            tool_rounds = tool_rounds.saturating_add(1);
                            if supervisor_run && tool_rounds > SUPERVISOR_MAX_TOOL_ROUNDS {
                                yield CatEvent::Error(AgentError::other(format!(
                                    "supervisor exceeded the maximum of {SUPERVISOR_MAX_TOOL_ROUNDS} tool rounds"
                                )));
                                return;
                            }
                            let (local, remaining): (Vec<_>, Vec<_>) = tool_calls
                                .iter()
                                .cloned()
                                .partition(|tc| tool_allowed(tool_allowlist.as_deref(), &tc.name) && self.local_tools.contains(&tc.name));
                            let (dynamic, external): (Vec<_>, Vec<_>) = remaining
                                .into_iter()
                                .partition(|tc| tool_allowed(tool_allowlist.as_deref(), &tc.name) && dynamic_tools.contains(&tc.name));

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
                                let mut approved_local = Vec::new();
                                for tc in &local {
                                    log_tool_call_started(
                                        "local",
                                        &state.thread_id.0,
                                        &state.run_id.0,
                                        tc,
                                    );
                                    yield CatEvent::ToolCall {
                                        id: tc.id.clone(),
                                        name: tc.name.clone(),
                                        args: tc.arguments.clone(),
                                    };
                                    let mut approval_request = approval_request_for_tool(
                                        tc,
                                        &state.thread_id.0,
                                        &state.run_id.0,
                                        state.config.metadata.as_ref(),
                                    );
                                    let (wait, approval_events) = self
                                        .approval_manager
                                        .start_request(approval_request.clone())
                                        .await;
                                    for event in approval_events {
                                        match event {
                                            ApprovalEvent::Requested(request) => {
                                                approval_request = request.clone();
                                                yield CatEvent::ToolApprovalRequested(request);
                                            }
                                            ApprovalEvent::Updated(request) => {
                                                approval_request = request.clone();
                                                yield CatEvent::ToolApprovalUpdated(request);
                                            }
                                            ApprovalEvent::Resolved { request, decision } => {
                                                approval_request = request.clone();
                                                yield CatEvent::ToolApprovalResolved { request, decision };
                                            }
                                        }
                                    }
                                    let approval_resolution = match wait {
                                        ApprovalWait::Immediate(resolution) => {
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                tool_call_id = %tc.id,
                                                tool_name = %tc.name,
                                                resolution = ?resolution,
                                                "tool_approval.ready"
                                            );
                                            resolution
                                        }
                                        ApprovalWait::Pending(rx) => {
                                            let should_wait = matches!(
                                                approval_request.platform.as_deref(),
                                                Some("web" | "tui")
                                            );
                                            let wait_started = Instant::now();
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                approval_id = %approval_request.id,
                                                tool_call_id = %tc.id,
                                                tool_name = %tc.name,
                                                platform = approval_request.platform.as_deref().unwrap_or(""),
                                                should_wait,
                                                "tool_approval.wait_start"
                                            );
                                            let decision = if should_wait {
                                                rx.await.unwrap_or(crate::approval::ToolApprovalDecision::Deny)
                                            } else {
                                                crate::approval::ToolApprovalDecision::Deny
                                            };
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                approval_id = %approval_request.id,
                                                tool_call_id = %tc.id,
                                                tool_name = %tc.name,
                                                decision = ?decision,
                                                waited_ms = wait_started.elapsed().as_millis() as u64,
                                                "tool_approval.wait_done"
                                            );
                                            let (resolution, event) = self
                                                .approval_manager
                                                .finish_request(&approval_request, decision)
                                                .await;
                                            if let ApprovalEvent::Resolved { request, decision } = event {
                                                yield CatEvent::ToolApprovalResolved { request, decision };
                                            }
                                            resolution
                                        }
                                    };
                                    if approval_resolution == ApprovalResolution::Approved {
                                        approved_local.push(tc.clone());
                                    } else {
                                        let result = "error: user denied tool execution".to_string();
                                        yield CatEvent::ToolCallResult {
                                            id: tc.id.clone(),
                                            name: tc.name.clone(),
                                            args: tc.arguments.clone(),
                                            result,
                                            success: false,
                                            elapsed_ms: 0,
                                        };
                                        all_outcomes.push(ToolCallOutcome::Error {
                                            tool_call_id: tc.id.clone(),
                                            tool_name: tc.name.clone(),
                                            error: "user denied tool execution".to_string(),
                                        });
                                    }
                                }

                                let resume_map = HashMap::new();
                                let started_at: HashMap<String, Instant> = approved_local
                                    .iter()
                                    .map(|tc| (tc.id.clone(), Instant::now()))
                                    .collect();
                                tracing::info!(
                                    thread_id = %state.thread_id.0,
                                    run_id = %state.run_id.0,
                                    tool_kind = "local",
                                    requested_count = local.len(),
                                    approved_count = approved_local.len(),
                                    tool_names = %approved_local.iter().map(|tc| tc.name.as_str()).collect::<Vec<_>>().join(","),
                                    "tool_batch.execute_start"
                                );
                                let batch_started = Instant::now();
                                let results = self
                                    .local_tools
                                    .execute_parallel(&approved_local, &resume_map, &tool_ctx)
                                    .await;
                                tracing::info!(
                                    thread_id = %state.thread_id.0,
                                    run_id = %state.run_id.0,
                                    tool_kind = "local",
                                    approved_count = approved_local.len(),
                                    elapsed_ms = batch_started.elapsed().as_millis() as u64,
                                    "tool_batch.execute_returned"
                                );

                                let (side_tx, mut side_rx) = mpsc::unbounded_channel();
                                let mut collect_fut = std::pin::pin!(collect_tool_results_parallel(
                                    results,
                                    &data_dir,
                                    overflow_bytes,
                                    Some(side_tx),
                                ));
                                let collected_results = loop {
                                    tokio::select! {
                                        Some(side_event) = side_rx.recv() => {
                                            yield side_event;
                                        }
                                        collected = &mut collect_fut => {
                                            break collected;
                                        }
                                    }
                                };

                                for (call_id, collected) in collected_results {
                                    let tc = approved_local.iter().find(|t| t.id == call_id).unwrap();
                                    let elapsed_ms = started_at
                                        .get(&call_id)
                                        .map(|instant| instant.elapsed().as_millis() as u64)
                                        .unwrap_or(0);
                                    let success = crate::tool_pretty::tool_success(&collected.preview);
                                    log_tool_call_finished(
                                        "local",
                                        &state.thread_id.0,
                                        &state.run_id.0,
                                        tc,
                                        &collected,
                                        success,
                                        elapsed_ms,
                                    );

                                    yield CatEvent::ToolCallResult {
                                        id: call_id.clone(),
                                        name: tc.name.clone(),
                                        args: tc.arguments.clone(),
                                        result: collected.preview.clone(),
                                        success,
                                        elapsed_ms,
                                    };

                                    for side_ev in make_side_events(tc, &collected.preview) {
                                        yield side_ev;
                                    }
                                    for side_ev in collected.side_events.clone() {
                                        yield side_ev;
                                    }

                                    all_outcomes.push(ToolCallOutcome::Result {
                                        tool_call_id: call_id,
                                        tool_name: tc.name.clone(),
                                        content: collected.content,
                                    });
                                }
                                state.user_state = tool_ctx.user_state.read().unwrap().clone();
                                yield CatEvent::StateUpdate(state.user_state.clone());
                            }

                            if !dynamic.is_empty() {
                                let mut approved_dynamic = Vec::new();
                                for tc in &dynamic {
                                    log_tool_call_started(
                                        "dynamic",
                                        &state.thread_id.0,
                                        &state.run_id.0,
                                        tc,
                                    );
                                    yield CatEvent::ToolCall {
                                        id: tc.id.clone(),
                                        name: tc.name.clone(),
                                        args: tc.arguments.clone(),
                                    };
                                    let mut approval_request = approval_request_for_tool(
                                        tc,
                                        &state.thread_id.0,
                                        &state.run_id.0,
                                        state.config.metadata.as_ref(),
                                    );
                                    let (wait, approval_events) = self
                                        .approval_manager
                                        .start_request(approval_request.clone())
                                        .await;
                                    for event in approval_events {
                                        match event {
                                            ApprovalEvent::Requested(request) => {
                                                approval_request = request.clone();
                                                yield CatEvent::ToolApprovalRequested(request);
                                            }
                                            ApprovalEvent::Updated(request) => {
                                                approval_request = request.clone();
                                                yield CatEvent::ToolApprovalUpdated(request);
                                            }
                                            ApprovalEvent::Resolved { request, decision } => {
                                                approval_request = request.clone();
                                                yield CatEvent::ToolApprovalResolved { request, decision };
                                            }
                                        }
                                    }
                                    let approval_resolution = match wait {
                                        ApprovalWait::Immediate(resolution) => {
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                tool_call_id = %tc.id,
                                                tool_name = %tc.name,
                                                resolution = ?resolution,
                                                "tool_approval.ready"
                                            );
                                            resolution
                                        }
                                        ApprovalWait::Pending(rx) => {
                                            let should_wait = matches!(
                                                approval_request.platform.as_deref(),
                                                Some("web" | "tui")
                                            );
                                            let wait_started = Instant::now();
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                approval_id = %approval_request.id,
                                                tool_call_id = %tc.id,
                                                tool_name = %tc.name,
                                                platform = approval_request.platform.as_deref().unwrap_or(""),
                                                should_wait,
                                                "tool_approval.wait_start"
                                            );
                                            let decision = if should_wait {
                                                rx.await.unwrap_or(crate::approval::ToolApprovalDecision::Deny)
                                            } else {
                                                crate::approval::ToolApprovalDecision::Deny
                                            };
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                approval_id = %approval_request.id,
                                                tool_call_id = %tc.id,
                                                tool_name = %tc.name,
                                                decision = ?decision,
                                                waited_ms = wait_started.elapsed().as_millis() as u64,
                                                "tool_approval.wait_done"
                                            );
                                            let (resolution, event) = self
                                                .approval_manager
                                                .finish_request(&approval_request, decision)
                                                .await;
                                            if let ApprovalEvent::Resolved { request, decision } = event {
                                                yield CatEvent::ToolApprovalResolved { request, decision };
                                            }
                                            resolution
                                        }
                                    };
                                    if approval_resolution == ApprovalResolution::Approved {
                                        approved_dynamic.push(tc.clone());
                                    } else {
                                        let result = "error: user denied tool execution".to_string();
                                        yield CatEvent::ToolCallResult {
                                            id: tc.id.clone(),
                                            name: tc.name.clone(),
                                            args: tc.arguments.clone(),
                                            result,
                                            success: false,
                                            elapsed_ms: 0,
                                        };
                                        all_outcomes.push(ToolCallOutcome::Error {
                                            tool_call_id: tc.id.clone(),
                                            tool_name: tc.name.clone(),
                                            error: "user denied tool execution".to_string(),
                                        });
                                    }
                                }

                                let resume_map = HashMap::new();
                                let started_at: HashMap<String, Instant> = approved_dynamic
                                    .iter()
                                    .map(|tc| (tc.id.clone(), Instant::now()))
                                    .collect();
                                tracing::info!(
                                    thread_id = %state.thread_id.0,
                                    run_id = %state.run_id.0,
                                    tool_kind = "dynamic",
                                    requested_count = dynamic.len(),
                                    approved_count = approved_dynamic.len(),
                                    tool_names = %approved_dynamic.iter().map(|tc| tc.name.as_str()).collect::<Vec<_>>().join(","),
                                    "tool_batch.execute_start"
                                );
                                let batch_started = Instant::now();
                                let results = dynamic_tools
                                    .execute_parallel(&approved_dynamic, &resume_map, &tool_ctx)
                                    .await;
                                tracing::info!(
                                    thread_id = %state.thread_id.0,
                                    run_id = %state.run_id.0,
                                    tool_kind = "dynamic",
                                    approved_count = approved_dynamic.len(),
                                    elapsed_ms = batch_started.elapsed().as_millis() as u64,
                                    "tool_batch.execute_returned"
                                );

                                let (side_tx, mut side_rx) = mpsc::unbounded_channel();
                                let mut collect_fut = std::pin::pin!(collect_tool_results_parallel(
                                    results,
                                    &data_dir,
                                    overflow_bytes,
                                    Some(side_tx),
                                ));
                                let collected_results = loop {
                                    tokio::select! {
                                        Some(side_event) = side_rx.recv() => {
                                            yield side_event;
                                        }
                                        collected = &mut collect_fut => {
                                            break collected;
                                        }
                                    }
                                };

                                for (call_id, collected) in collected_results {
                                    let tc = approved_dynamic.iter().find(|t| t.id == call_id).unwrap();
                                    let elapsed_ms = started_at
                                        .get(&call_id)
                                        .map(|instant| instant.elapsed().as_millis() as u64)
                                        .unwrap_or(0);
                                    let success = crate::tool_pretty::tool_success(&collected.preview);
                                    log_tool_call_finished(
                                        "dynamic",
                                        &state.thread_id.0,
                                        &state.run_id.0,
                                        tc,
                                        &collected,
                                        success,
                                        elapsed_ms,
                                    );

                                    yield CatEvent::ToolCallResult {
                                        id: call_id.clone(),
                                        name: tc.name.clone(),
                                        args: tc.arguments.clone(),
                                        result: collected.preview.clone(),
                                        success,
                                        elapsed_ms,
                                    };
                                    for side_ev in collected.side_events.clone() {
                                        yield side_ev;
                                    }

                                    all_outcomes.push(ToolCallOutcome::Result {
                                        tool_call_id: call_id,
                                        tool_name: tc.name.clone(),
                                        content: collected.content,
                                    });
                                }
                            }

                            if !external.is_empty() {
                                let names = external
                                    .iter()
                                    .map(|t| t.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                tracing::warn!(
                                    thread_id = %state.thread_id.0,
                                    run_id = %state.run_id.0,
                                    tool_names = %names,
                                    tool_count = external.len(),
                                    "tool_call.failed"
                                );
                                yield CatEvent::Error(AgentError::tool(
                                    "external",
                                    format!("unhandled external tools: {names}"),
                                ));
                                return;
                            }

                            refresh_runtime_tool_definitions(
                                &mut state,
                                &self.local_tools,
                                &dynamic_tools,
                                tool_allowlist.as_deref(),
                            );

                            next_input = Some(LoopInput::Resume {
                                state,
                                results: all_outcomes,
                            });
                            break;
                        }
                        AgentEvent::Checkpoint(cp) => {
                            last_messages = cp.state.messages.clone();
                            last_user_state = cp.state.user_state.clone();
                        }
                        AgentEvent::Done => {
                            let elapsed_ms = run_start.elapsed().as_millis() as u64;
                            tracing::info!(
                                thread_id = %log_thread_id,
                                run_id = %log_run_id,
                                supervisor_run,
                                tool_rounds,
                                prompt_tokens = total_prompt_tokens,
                                completion_tokens = total_completion_tokens,
                                max_prompt_tokens,
                                elapsed_ms,
                                "agent_run.completed"
                            );
                            yield CatEvent::Stats {
                                prompt_tokens: total_prompt_tokens,
                                completion_tokens: total_completion_tokens,
                                max_prompt_tokens,
                                elapsed_ms,
                            };
                            yield CatEvent::History(last_messages.clone(), last_user_state.clone());
                            yield CatEvent::Done;
                            return;
                        }
                        AgentEvent::Error(e) => {
                            tracing::warn!(
                                thread_id = %log_thread_id,
                                run_id = %log_run_id,
                                supervisor_run,
                                tool_rounds,
                                prompt_tokens = total_prompt_tokens,
                                completion_tokens = total_completion_tokens,
                                max_prompt_tokens,
                                elapsed_ms = run_start.elapsed().as_millis() as u64,
                                error = %e,
                                "agent_run.failed"
                            );
                            yield CatEvent::Error(e);
                            return;
                        }
                        AgentEvent::Cancelled => {
                            tracing::info!(
                                thread_id = %log_thread_id,
                                run_id = %log_run_id,
                                supervisor_run,
                                tool_rounds,
                                elapsed_ms = run_start.elapsed().as_millis() as u64,
                                "agent_run.cancelled"
                            );
                            yield CatEvent::Done;
                            return;
                        }
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

fn build_tool_definition_ctx(input: &LoopInput) -> ToolDefinitionContext {
    match input {
        LoopInput::Start {
            metadata,
            user_state,
            ..
        } => ToolDefinitionContext {
            metadata: metadata.clone(),
            user_state: user_state.clone().unwrap_or(serde_json::Value::Null),
            ..ToolDefinitionContext::default()
        },
        LoopInput::Resume { state, .. } | LoopInput::Cancel { state } => ToolDefinitionContext {
            thread_id: Some(state.thread_id.clone()),
            run_id: Some(state.run_id.clone()),
            metadata: state.config.metadata.clone(),
            user_state: state.user_state.clone(),
        },
    }
}

fn merge_runtime_tool_definitions(
    local_tools: &DefaultToolRegistry,
    dynamic_tools: &DefaultToolRegistry,
    ctx: &ToolDefinitionContext,
    external_defs: &[ToolDefinition],
    tool_allowlist: Option<&[String]>,
) -> Vec<ToolDefinition> {
    let mut defs = local_tools.definitions_with_context(ctx);
    defs.extend(dynamic_tools.definitions_with_context(ctx));
    defs.extend_from_slice(external_defs);
    let defs = filter_trigger_management_tool_definitions(ctx, defs);
    filter_tool_allowlist(tool_allowlist, defs)
}

fn refresh_runtime_tool_definitions(
    state: &mut remi_agentloop::prelude::AgentState,
    local_tools: &DefaultToolRegistry,
    dynamic_tools: &DefaultToolRegistry,
    tool_allowlist: Option<&[String]>,
) {
    let external_defs: Vec<_> = state
        .tool_definitions
        .iter()
        .filter(|definition| {
            let name = definition.function.name.as_str();
            !local_tools.contains(name) && !dynamic_tools.contains(name)
        })
        .cloned()
        .collect();

    let ctx = ToolDefinitionContext {
        thread_id: Some(state.thread_id.clone()),
        run_id: Some(state.run_id.clone()),
        metadata: state.config.metadata.clone(),
        user_state: state.user_state.clone(),
    };

    state.tool_definitions = merge_runtime_tool_definitions(
        local_tools,
        dynamic_tools,
        &ctx,
        &external_defs,
        tool_allowlist,
    );
}

fn filter_trigger_management_tool_definitions(
    ctx: &ToolDefinitionContext,
    defs: Vec<ToolDefinition>,
) -> Vec<ToolDefinition> {
    if !crate::suppress_trigger_management(ctx.metadata.as_ref()) {
        return defs;
    }

    defs.into_iter()
        .filter(|definition| {
            !TRIGGER_MANAGEMENT_TOOL_NAMES.contains(&definition.function.name.as_str())
        })
        .collect()
}

fn filter_tool_allowlist(
    tool_allowlist: Option<&[String]>,
    defs: Vec<ToolDefinition>,
) -> Vec<ToolDefinition> {
    let Some(tool_allowlist) = tool_allowlist else {
        return defs;
    };
    defs.into_iter()
        .filter(|definition| tool_allowed(Some(tool_allowlist), &definition.function.name))
        .collect()
}

fn tool_allowed(tool_allowlist: Option<&[String]>, name: &str) -> bool {
    tool_allowlist
        .map(|allowlist| allowlist.iter().any(|tool| tool == name))
        .unwrap_or(true)
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

fn approval_request_for_tool(
    tc: &ParsedToolCall,
    session_id: &str,
    run_id: &str,
    metadata: Option<&serde_json::Value>,
) -> ToolApprovalRequest {
    let internal_supervisor = crate::metadata_flag_enabled(metadata, "supervisor_run");
    let approval_session_id = metadata
        .and_then(|value| value.get("thread_id"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(session_id);
    ToolApprovalRequest {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: approval_session_id.to_string(),
        run_id: run_id.to_string(),
        tool_call_id: tc.id.clone(),
        tool_name: tc.name.clone(),
        risk: if internal_supervisor {
            crate::approval::ToolRiskLevel::Low
        } else {
            classify_tool_risk(&tc.name, &tc.arguments)
        },
        args_summary: summarize_tool_args(&tc.arguments),
        platform: metadata
            .and_then(|value| value.get("platform"))
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        review: None,
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

#[derive(Debug, Clone)]
struct CollectedToolResult {
    content: Content,
    preview: String,
    side_events: Vec<CatEvent>,
}

impl CollectedToolResult {
    fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            content: Content::text(text.clone()),
            preview: text,
            side_events: Vec::new(),
        }
    }
}

fn preview_text_for_content(content: &Content) -> String {
    let text = content.text_content();
    if !text.is_empty() {
        return text;
    }

    match content {
        Content::Text(_) => String::new(),
        Content::Parts(parts) => {
            let mut image_count = 0usize;
            let mut audio_count = 0usize;
            let mut file_count = 0usize;

            for part in parts {
                match part {
                    ContentPart::Text { .. } => {}
                    ContentPart::ImageUrl { .. } | ContentPart::ImageBase64 { .. } => {
                        image_count += 1;
                    }
                    ContentPart::Audio { .. } => {
                        audio_count += 1;
                    }
                    ContentPart::File { .. } => {
                        file_count += 1;
                    }
                }
            }

            let mut segments = Vec::new();
            if image_count > 0 {
                segments.push(format!(
                    "{image_count} image{}",
                    if image_count == 1 { "" } else { "s" }
                ));
            }
            if audio_count > 0 {
                segments.push(format!(
                    "{audio_count} audio part{}",
                    if audio_count == 1 { "" } else { "s" }
                ));
            }
            if file_count > 0 {
                segments.push(format!(
                    "{file_count} file{}",
                    if file_count == 1 { "" } else { "s" }
                ));
            }

            if segments.is_empty() {
                String::new()
            } else {
                format!("[multimodal tool result: {}]", segments.join(", "))
            }
        }
    }
}

async fn collect_tool_results_parallel<'a>(
    results: Vec<(String, Result<BoxedToolResult<'a>, AgentError>)>,
    data_dir: &Path,
    overflow_bytes: usize,
    side_event_tx: Option<mpsc::UnboundedSender<CatEvent>>,
) -> Vec<(String, CollectedToolResult)> {
    let data_dir = data_dir.to_path_buf();
    let futures = results.into_iter().map(|(call_id, result)| {
        let data_dir = data_dir.clone();
        let side_event_tx = side_event_tx.clone();
        async move {
            let collected = collect_result_with_overflow(
                result,
                &data_dir,
                overflow_bytes,
                side_event_tx,
                Some(call_id.as_str()),
            )
            .await;
            (call_id, collected)
        }
    });
    join_all(futures).await
}

async fn collect_result(
    result: Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>,
    side_event_tx: Option<mpsc::UnboundedSender<CatEvent>>,
    parent_tool_call_id: Option<&str>,
) -> CollectedToolResult {
    match result {
        Err(e) => CollectedToolResult::text(format!("error: {e}")),
        Ok(ToolResult::Interrupt(_)) => CollectedToolResult::text("interrupted"),
        Ok(ToolResult::Output(s)) => {
            let mut s = std::pin::pin!(s);
            let mut last = Content::text(String::new());
            let mut side_events = Vec::new();
            while let Some(out) = s.next().await {
                match out {
                    ToolOutput::Result(c) => last = c,
                    ToolOutput::SubSession(mut event) => {
                        if event.parent_tool_call_id.is_empty() {
                            if let Some(parent) = parent_tool_call_id {
                                event.parent_tool_call_id = parent.to_string();
                            }
                        }
                        let event = CatEvent::SubSession(event);
                        if let Some(tx) = &side_event_tx {
                            let _ = tx.send(event);
                        } else {
                            side_events.push(event);
                        }
                    }
                    _ => {}
                }
            }
            CollectedToolResult {
                preview: preview_text_for_content(&last),
                content: last,
                side_events,
            }
        }
    }
}

async fn collect_result_with_overflow(
    result: Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>,
    data_dir: &Path,
    overflow_bytes: usize,
    side_event_tx: Option<mpsc::UnboundedSender<CatEvent>>,
    parent_tool_call_id: Option<&str>,
) -> CollectedToolResult {
    let collected = collect_result(result, side_event_tx, parent_tool_call_id).await;
    if collected.content.is_multimodal() {
        return collected;
    }

    let side_events = collected.side_events.clone();
    let text = collected.content.text_content();
    if text.len() <= overflow_bytes {
        return CollectedToolResult {
            side_events,
            ..CollectedToolResult::text(text)
        };
    }

    let tmp_dir = data_dir.join("tmp");
    let _ = tokio::fs::create_dir_all(&tmp_dir).await;
    let filename = format!("tool_out_{}.txt", uuid::Uuid::new_v4());
    let file_path = tmp_dir.join(&filename);
    let total = text.len();
    let mut result = match tokio::fs::write(&file_path, text.as_bytes()).await {
        Ok(()) => CollectedToolResult::text(format!(
            "[Output too large ({total} bytes) — saved to tmp/{filename}]\n\
             Use fs_read with path=\"tmp/{filename}\" (offset=0, length={}) \
             and increment offset until remaining=0.",
            crate::tools::DEFAULT_FS_READ_LENGTH
        )),
        Err(_) => CollectedToolResult::text(text),
    };
    result.side_events = side_events;
    result
}

fn make_side_events(tc: &ParsedToolCall, result_str: &str) -> Vec<CatEvent> {
    let mut evs = Vec::new();
    if let Some(ev) = skill::make_skill_event(tc, result_str) {
        evs.push(CatEvent::Skill(ev));
    }
    for ev in todo::make_todo_events(tc, result_str) {
        evs.push(CatEvent::Todo(ev));
    }
    for ev in trigger::make_trigger_events(tc, result_str) {
        evs.push(CatEvent::Trigger(ev));
    }
    evs
}

#[cfg(test)]
mod tests {
    use super::{
        approval_request_for_tool, collect_result_with_overflow, collect_tool_results_parallel,
        CatAgent,
    };
    use crate::approval::ToolApprovalManager;
    use crate::events::CatEvent;
    use futures::{stream, Stream, StreamExt};
    use remi_agentloop::prelude::{
        Agent, AgentError, AgentState, Content, ContentPart, LoopInput, ParsedToolCall, StepConfig,
        ToolCallOutcome, ToolContext, ToolOutput, ToolResult,
    };
    use remi_agentloop::tool::{
        registry::DefaultToolRegistry, BoxedToolResult, BoxedToolStream, Tool,
    };
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("remi-agent-result-test-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn approval_request_uses_outer_thread_id_from_metadata() {
        let tc = ParsedToolCall {
            id: "call-1".to_string(),
            name: "manage_yourself".to_string(),
            arguments: serde_json::json!({"command": "profile list"}),
        };

        let request = approval_request_for_tool(
            &tc,
            "agentloop-inner-thread",
            "run-1",
            Some(&serde_json::json!({
                "thread_id": "tui-session-1",
                "platform": "tui"
            })),
        );

        assert_eq!(request.session_id, "tui-session-1");
        assert_eq!(request.platform.as_deref(), Some("tui"));
    }

    #[tokio::test]
    async fn multimodal_results_are_preserved_for_resume() {
        let root = test_root();
        let content = Content::parts(vec![
            ContentPart::text("image summary"),
            ContentPart::image_url("data:image/png;base64,abc"),
        ]);
        let collected = collect_result_with_overflow(
            Ok(ToolResult::Output(stream::iter(vec![ToolOutput::Result(
                content.clone(),
            )]))),
            &root,
            8,
            None,
            None,
        )
        .await;

        assert_eq!(collected.preview, "image summary");
        match collected.content {
            Content::Parts(parts) => {
                assert!(matches!(
                    parts.get(1),
                    Some(ContentPart::ImageUrl { image_url })
                        if image_url.url == "data:image/png;base64,abc"
                ));
            }
            other => panic!("expected multimodal content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn large_text_results_still_spill_to_tmp_file() {
        let root = test_root();
        let collected = collect_result_with_overflow(
            Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                "x".repeat(64),
            )]))),
            &root,
            8,
            None,
            None,
        )
        .await;

        let preview = collected.preview;
        assert!(preview.contains("[Output too large (64 bytes)"));
        assert!(matches!(collected.content, Content::Text(ref text) if text == &preview));

        let mut entries = tokio::fs::read_dir(root.join("tmp"))
            .await
            .expect("tmp dir should exist after overflow spill");
        assert!(entries
            .next_entry()
            .await
            .expect("tmp dir should be readable")
            .is_some());
    }

    fn delayed_boxed_result(
        label: &'static str,
        delay: Duration,
    ) -> Result<BoxedToolResult<'static>, remi_agentloop::prelude::AgentError> {
        let output: BoxedToolStream<'static> = Box::pin(async_stream::stream! {
            tokio::time::sleep(delay).await;
            yield ToolOutput::text(label);
        });
        Ok(ToolResult::Output(output))
    }

    #[tokio::test]
    async fn tool_result_streams_are_collected_in_parallel() {
        let root = test_root();
        let started = Instant::now();
        let results = collect_tool_results_parallel(
            vec![
                (
                    "call_a".to_string(),
                    delayed_boxed_result("a", Duration::from_millis(300)),
                ),
                (
                    "call_b".to_string(),
                    delayed_boxed_result("b", Duration::from_millis(300)),
                ),
            ],
            &root,
            8_192,
            None,
        )
        .await;

        assert_eq!(results.len(), 2);
        assert!(started.elapsed() < Duration::from_millis(550));
        assert_eq!(results[0].1.preview, "a");
        assert_eq!(results[1].1.preview, "b");
    }

    #[tokio::test]
    async fn sub_session_events_are_forwarded_before_tool_result_completes() {
        let root = test_root();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let output: BoxedToolStream<'static> = Box::pin(async_stream::stream! {
            yield ToolOutput::SubSession(remi_agentloop::prelude::SubSessionEvent::new(
                "",
                remi_agentloop::prelude::ThreadId("sub-thread".to_string()),
                remi_agentloop::prelude::RunId("sub-run".to_string()),
                "coder",
                None,
                1,
                remi_agentloop::prelude::SubSessionEventPayload::ToolCallStart {
                    id: "sub-call".to_string(),
                    name: "fs_ls".to_string(),
                },
            ));
            tokio::time::sleep(Duration::from_millis(200)).await;
            yield ToolOutput::text("done");
        });
        let mut collect_fut = std::pin::pin!(collect_tool_results_parallel(
            vec![("call".to_string(), Ok(ToolResult::Output(output)))],
            &root,
            8_192,
            Some(tx),
        ));

        let side_event = tokio::select! {
            Some(event) = rx.recv() => event,
            _ = &mut collect_fut => panic!("tool collection completed before side event"),
        };
        assert!(matches!(
            side_event,
            CatEvent::SubSession(event) if event.parent_tool_call_id == "call"
        ));

        let results = collect_fut.await;
        assert_eq!(results[0].1.preview, "done");
        assert!(results[0].1.side_events.is_empty());
    }

    struct ParallelToolInnerAgent;

    impl Agent for ParallelToolInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start { .. } => Ok(stream::iter(vec![
                    remi_agentloop::types::AgentEvent::NeedToolExecution {
                        state: AgentState::new(StepConfig::new("test-model")),
                        tool_calls: vec![
                            ParsedToolCall {
                                id: "call_a".to_string(),
                                name: "lazy_wait".to_string(),
                                arguments: serde_json::json!({ "label": "a" }),
                            },
                            ParsedToolCall {
                                id: "call_b".to_string(),
                                name: "lazy_wait".to_string(),
                                arguments: serde_json::json!({ "label": "b" }),
                            },
                        ],
                        completed_results: vec![],
                    },
                ])),
                LoopInput::Resume { results, .. } => {
                    let labels = results
                        .iter()
                        .filter_map(|result| match result {
                            ToolCallOutcome::Result { content, .. } => Some(content.text_content()),
                            ToolCallOutcome::Error { .. } => None,
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    Ok(stream::iter(vec![
                        remi_agentloop::types::AgentEvent::TextDelta(labels),
                        remi_agentloop::types::AgentEvent::Done,
                    ]))
                }
                LoopInput::Cancel { .. } => {
                    Ok(stream::iter(vec![remi_agentloop::types::AgentEvent::Done]))
                }
            }
        }
    }

    struct LazyWaitTool;

    impl Tool for LazyWaitTool {
        fn name(&self) -> &str {
            "lazy_wait"
        }

        fn description(&self) -> &str {
            "Waits before returning the supplied label."
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "label": { "type": "string" }
                },
                "required": ["label"]
            })
        }

        async fn execute(
            &self,
            arguments: serde_json::Value,
            _resume: Option<remi_agentloop::types::ResumePayload>,
            _ctx: &ToolContext,
        ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
            let label = arguments
                .get("label")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            Ok(ToolResult::Output(async_stream::stream! {
                tokio::time::sleep(Duration::from_millis(300)).await;
                yield ToolOutput::text(label);
            }))
        }
    }

    struct EndlessToolInnerAgent;

    impl Agent for EndlessToolInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            let state = match req {
                LoopInput::Start { metadata, .. } => {
                    let mut state = AgentState::new(StepConfig::new("test-model"));
                    state.config.metadata = metadata;
                    state
                }
                LoopInput::Resume { state, .. } | LoopInput::Cancel { state } => state,
            };
            Ok(stream::iter(vec![
                remi_agentloop::types::AgentEvent::NeedToolExecution {
                    state,
                    tool_calls: vec![ParsedToolCall {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: "instant".to_string(),
                        arguments: serde_json::json!({}),
                    }],
                    completed_results: vec![],
                },
            ]))
        }
    }

    struct InstantTool;

    impl Tool for InstantTool {
        fn name(&self) -> &str {
            "instant"
        }

        fn description(&self) -> &str {
            "Returns immediately."
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _resume: Option<remi_agentloop::types::ResumePayload>,
            _ctx: &ToolContext,
        ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
            Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                "ok",
            )])))
        }
    }

    #[tokio::test]
    async fn cat_agent_collects_parallel_tool_streams_end_to_end() {
        let mut local_tools = DefaultToolRegistry::new();
        local_tools.register(LazyWaitTool);
        let agent = CatAgent {
            inner: ParallelToolInnerAgent,
            local_tools,
            data_dir: test_root(),
            overflow_bytes: 8_192,
            im_bridge: None,
            tool_allowlist: None,
            approval_manager: ToolApprovalManager::new(),
        };

        let started = Instant::now();
        let events = agent
            .stream_with_input(LoopInput::start("run parallel tools"))
            .collect::<Vec<_>>()
            .await;

        assert!(started.elapsed() < Duration::from_millis(550));
        assert!(events
            .iter()
            .any(|event| matches!(event, CatEvent::Text(text) if text == "a,b")));
    }

    #[tokio::test]
    async fn supervisor_run_stops_after_bounded_tool_rounds() {
        let mut local_tools = DefaultToolRegistry::new();
        local_tools.register(InstantTool);
        let agent = CatAgent {
            inner: EndlessToolInnerAgent,
            local_tools,
            data_dir: test_root(),
            overflow_bytes: 8_192,
            im_bridge: None,
            tool_allowlist: None,
            approval_manager: ToolApprovalManager::new(),
        };

        let events = agent
            .stream_with_input(
                LoopInput::start("keep calling tools")
                    .metadata(serde_json::json!({"supervisor_run": "true"})),
            )
            .collect::<Vec<_>>()
            .await;

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, CatEvent::ToolCall { .. }))
                .count(),
            8
        );
        assert!(events.iter().any(|event| {
            matches!(event, CatEvent::Error(error) if error.to_string().contains("maximum of 8 tool rounds"))
        }));
    }
}
