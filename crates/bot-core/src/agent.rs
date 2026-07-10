//! Core drive loop for `CatBot`.
//!
//! `run()` wraps any `AgentLoop` (or composable inner agent), registers
//! skill and todo tools locally, drives the inner agent, handles
//! `NeedToolExecution` by executing local tools and resuming, and finally
//! emits `CatEvent::History` + `CatEvent::Done` when the run finishes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_stream::stream;
use futures::{
    future::{FutureExt, LocalBoxFuture},
    stream::FuturesUnordered,
    Stream, StreamExt,
};
use tracing::debug;

use bot_runtime_core::ToolContext;
use bot_runtime_core::{
    build_tool_definition_ctx, chat_ctx_from_input, inject_extra_tools,
    tool_ctx_from_state_with_cancel, CoreAgentLoop, CoreCancelKind, CoreDriveConfig,
    CoreDriveEvent, CoreSteerBatch, CoreStreamOptions, CoreUsageStats,
};
use remi_agentloop::prelude::{
    AgentError, AgentState, CancellationToken, Content, ContentPart, LoopInput, Message,
    ParsedToolCall, ToolCallOutcome, ToolDefinition, ToolDefinitionContext, ToolOutput, ToolResult,
};
use remi_agentloop::tool::registry::{DefaultToolRegistry, ToolRegistry};
use remi_agentloop::tool::BoxedToolResult;
use remi_agentloop::types::{AgentEvent, ResumePayload};
use tokio::sync::{mpsc, oneshot};

use crate::approval::{
    classify_tool_risk_assessment, command_key, review_tool_risk, summarize_tool_args,
    ApprovalEvent, ApprovalResolution, ApprovalWait, ModelApprovalReviewer, ToolApprovalDecision,
    ToolApprovalManager, ToolApprovalRequest, ToolRiskAssessment, ToolRiskLevel, ToolRiskReview,
};
use crate::events::CatEvent;
use crate::hooks::{
    remi_tool_input_from_hook_update, HookContext, HookEventName, HookManager,
    HookPermissionDecision, ToolHookContext,
};
use crate::im_tools::{register_im_tools, ImFileBridge};
use crate::skill;
use crate::todo;
use crate::user_question::UserQuestionManager;

const SUPERVISOR_MAX_TOOL_ROUNDS: usize = 8;
const INTERRUPTED_TOOL_RESULT_ERROR: &str = "tool execution was interrupted before completion";

#[derive(Debug, Clone, Copy)]
enum UnavailableToolReason {
    NotFound,
    NotAllowed,
}

impl UnavailableToolReason {
    fn error_message(self, tool_name: &str) -> String {
        match self {
            Self::NotFound => format!("error: tool not found: {tool_name}"),
            Self::NotAllowed => {
                format!("error: tool is not allowed for current agent: {tool_name}")
            }
        }
    }

    fn tool_kind(self) -> &'static str {
        match self {
            Self::NotFound => "missing",
            Self::NotAllowed => "not_allowed",
        }
    }
}

fn stats_event(stats: CoreUsageStats) -> CatEvent {
    CatEvent::Stats {
        prompt_tokens: stats.prompt_tokens,
        completion_tokens: stats.completion_tokens,
        max_prompt_tokens: stats.max_prompt_tokens,
        elapsed_ms: stats.elapsed_ms,
    }
}

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

fn is_denied_approval_event(event: &CatEvent) -> bool {
    matches!(
        event,
        CatEvent::ToolApprovalResolved {
            decision: ToolApprovalDecision::Deny,
            ..
        }
    )
}

/// Returns true when the event indicates an approval or user question is now
/// pending (waiting for human response). While pending, foreground tool
/// timeouts must be suppressed so the tool is not moved to the background
/// before the human can respond.
fn is_approval_request_event(event: &CatEvent) -> bool {
    matches!(
        event,
        CatEvent::ToolApprovalRequested(..) | CatEvent::UserQuestionRequested(..)
    )
}

fn is_approval_resolved_event(event: &CatEvent) -> bool {
    matches!(
        event,
        CatEvent::ToolApprovalResolved { .. } | CatEvent::UserQuestionResolved { .. }
    )
}

// ── CatAgent ─────────────────────────────────────────────────────────────────

/// Drives an inner agent loop, handling skill and todo tools locally.
pub struct CatAgent<I> {
    /// The inner agent (AgentLoop or composed layers).
    pub inner: I,
    /// Locally-handled tools shared across model/runtime variants.
    pub local_tools: Arc<DefaultToolRegistry>,
    /// Model-bound local tools, such as delegate agents with baked-in model options.
    pub model_tools: Option<DefaultToolRegistry>,
    /// Workspace root — oversized tool outputs are spilled here.
    pub data_dir: PathBuf,
    /// Workspace root used by workspace file tools.
    pub workspace_root: PathBuf,
    /// User/container-facing workspace root label.
    pub workspace_root_label: String,
    /// Whether local file tools may accept host absolute paths.
    pub allow_host_absolute_paths: bool,
    /// Tool-output byte threshold above which output is spilled to a temp file.
    /// Configured from the model profile; can be overridden per-builder.
    pub overflow_bytes: usize,
    /// Optional daemon-mediated IM bridge used for per-platform upload/download tools.
    pub im_bridge: Option<Arc<dyn ImFileBridge>>,
    /// Optional explicit tool allowlist from the active agent profile.
    pub tool_allowlist: Option<Vec<String>>,
    /// Shared in-memory approval manager for risky tool calls.
    pub approval_manager: Arc<ToolApprovalManager>,
    /// Optional model-backed reviewer for tool calls not covered by hard-coded rules.
    pub approval_reviewer: Option<Arc<ModelApprovalReviewer>>,
    /// Shared in-memory user-question manager for interactive clarification.
    pub user_question_manager: Arc<UserQuestionManager>,
    /// Codex-compatible hook runner.
    pub hook_manager: Arc<HookManager>,
    /// Background tool task registry.
    pub tool_tasks: Arc<crate::tool_tasks::ToolTaskManager>,
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
        self.stream_with_input_and_tool_allowlist_and_options(
            input,
            None,
            CoreStreamOptions::default(),
        )
    }

    pub fn stream_with_input_and_tool_allowlist<'a>(
        &'a self,
        input: LoopInput,
        tool_allowlist_override: Option<Vec<String>>,
    ) -> impl Stream<Item = CatEvent> + 'a {
        self.stream_with_input_and_tool_allowlist_and_options(
            input,
            tool_allowlist_override,
            CoreStreamOptions::default(),
        )
    }

    pub fn stream_with_input_and_options<'a>(
        &'a self,
        input: LoopInput,
        options: CoreStreamOptions,
    ) -> impl Stream<Item = CatEvent> + 'a {
        self.stream_with_input_and_tool_allowlist_and_options(input, None, options)
    }

    pub fn stream_with_input_and_tool_allowlist_and_options<'a>(
        &'a self,
        input: LoopInput,
        tool_allowlist_override: Option<Vec<String>>,
        options: CoreStreamOptions,
    ) -> impl Stream<Item = CatEvent> + 'a {
        let data_dir = self.data_dir.clone();
        let workspace_root = self.workspace_root.clone();
        let workspace_root_label = self.workspace_root_label.clone();
        let allow_host_absolute_paths = self.allow_host_absolute_paths;
        let overflow_bytes = self.overflow_bytes;
        let tool_allowlist = tool_allowlist_override.or_else(|| self.tool_allowlist.clone());
        let hook_manager = Arc::clone(&self.hook_manager);
        let approval_reviewer = self.approval_reviewer.clone();
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
            workspace_root.clone(),
            workspace_root_label,
            allow_host_absolute_paths,
            self.im_bridge.clone(),
        );
        let dynamic_tool_count = dynamic_tools.definitions_with_context(&tool_def_ctx).len();
        let extra_defs = merge_runtime_tool_definitions(
            &self.local_tools,
            self.model_tools.as_ref(),
            &dynamic_tools,
            &tool_def_ctx,
            &[],
            tool_allowlist.as_deref(),
        );
        let tool_names = extra_defs
            .iter()
            .map(|definition| definition.function.name.clone())
            .collect::<Vec<_>>();

        stream! {
            let run_start = Instant::now();
            let cancel_signal = options.cancel.clone();
            tracing::info!(
                thread_id = %log_thread_id,
                run_id = %log_run_id,
                supervisor_run,
                dynamic_tools = dynamic_tool_count,
                tool_count = tool_names.len(),
                tools = ?tool_names,
                allowlist = ?tool_allowlist,
                "agent_run.start"
            );
            let mut agent_loop = CoreAgentLoop::new(CoreDriveConfig {
                cancel: cancel_signal.clone(),
                max_tool_rounds: supervisor_run.then_some(SUPERVISOR_MAX_TOOL_ROUNDS),
            });

            let mut current = input;
            let mut last_messages: Vec<Message> = vec![];
            let mut last_user_state: serde_json::Value = serde_json::Value::Null;

            loop {
                let run_input = inject_extra_tools(current, extra_defs.clone());
                let run_ctx = chat_ctx_from_input(&run_input, cancel_signal.clone());
                let inner_stream = match self.inner.chat(run_ctx, run_input).await {
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
                let mut inner_stream = std::pin::pin!(agent_loop.drive(inner_stream));
                let mut next_input: Option<LoopInput> = None;
                let mut last_checkpoint_state: Option<remi_agentloop::prelude::AgentState> = None;

                loop {
                    let ev = inner_stream.next().await;
                    let Some(ev) = ev else {
                        break;
                    };
                    match ev {
                        CoreDriveEvent::Text(t) => {
                            yield CatEvent::Text(t);
                            if let (Some(steer), Some(state)) =
                                (options.steer.as_ref(), last_checkpoint_state.clone())
                            {
                                if let Some(batch) = steer.drain_batch() {
                                    yield CatEvent::SteerInjected(crate::SteerInjectedEvent {
                                        steer_ids: batch.ids.clone(),
                                        session_id: state.thread_id.0.clone(),
                                        preview: batch.preview.clone(),
                                        count: batch.count,
                                    });
                                    next_input = Some(steer_start_input(state, batch));
                                    break;
                                }
                            }
                        }
                        CoreDriveEvent::Thinking(content) => {
                            yield CatEvent::Thinking(content);
                        }
                        CoreDriveEvent::ThinkingDelta(delta) => {
                            yield CatEvent::Thinking(delta);
                        }
                        CoreDriveEvent::ToolCallStart { id, name } => {
                            yield CatEvent::ToolCallStart { id, name };
                        }
                        CoreDriveEvent::ToolCallArgumentsDelta { id, delta } => {
                            yield CatEvent::ToolCallArgumentsDelta { id, delta };
                        }
                        CoreDriveEvent::Usage(stats) => {
                            yield stats_event(stats);
                        }
                        CoreDriveEvent::ToolDispatch(bot_runtime_core::CoreToolDispatch {
                            mut state,
                            tool_calls,
                            completed_results,
                            stats,
                            ..
                        }) => {
                            let mut local = Vec::new();
                            let mut dynamic = Vec::new();
                            let mut unavailable = Vec::new();
                            for tc in tool_calls.iter().cloned() {
                                let is_runtime_tool = runtime_tools_contains(
                                    &self.local_tools,
                                    self.model_tools.as_ref(),
                                    &tc.name,
                                );
                                let is_dynamic_tool = dynamic_tools.contains(&tc.name);
                                if !is_runtime_tool && !is_dynamic_tool {
                                    unavailable.push((tc, UnavailableToolReason::NotFound));
                                } else if !tool_allowed(tool_allowlist.as_deref(), &tc.name) {
                                    unavailable.push((tc, UnavailableToolReason::NotAllowed));
                                } else if is_runtime_tool {
                                    local.push(tc);
                                } else {
                                    dynamic.push(tc);
                                }
                            }

                            if !local.is_empty() {
                                let names: Vec<&str> = local.iter().map(|t| t.name.as_str()).collect();
                                debug!(tools = ?names, "agent: dispatching local tools");
                            }
                            if !dynamic.is_empty() {
                                let names: Vec<&str> = dynamic.iter().map(|t| t.name.as_str()).collect();
                                debug!(tools = ?names, "agent: dispatching dynamic tools");
                            }

                            let tool_ctx =
                                tool_ctx_from_state_with_cancel(&state, cancel_signal.clone());
                            let mut all_outcomes: Vec<ToolCallOutcome> = completed_results;

                            if !local.is_empty() {
                                let mut approved_local = Vec::new();
                                for original_tc in &local {
                                    let mut tc = original_tc.clone();
                                    let hook_context = hook_context_from_state(
                                        &state,
                                        &workspace_root,
                                    );
                                    let pre_hook = hook_manager
                                        .run_tool(
                                            HookEventName::PreToolUse,
                                            &hook_context,
                                            &tool_hook_context(&tc, None),
                                        )
                                        .await;
                                    if let Some(updated_input) = pre_hook.updated_input {
                                        tc.arguments = remi_tool_input_from_hook_update(
                                            &tc.name,
                                            &tc.arguments,
                                            updated_input,
                                        );
                                    }
                                    if pre_hook.blocked || pre_hook.permission_decision == Some(HookPermissionDecision::Deny) {
                                        let result = pre_hook
                                            .reason
                                            .unwrap_or_else(|| "error: tool execution blocked by hook".to_string());
                                        yield CatEvent::ToolCallResult {
                                            id: tc.id.clone(),
                                            name: tc.name.clone(),
                                            args: tc.arguments.clone(),
                                            result: result.clone(),
                                            success: false,
                                            elapsed_ms: 0,
                                        };
                                        yield stats_event(stats);
                                        all_outcomes.push(ToolCallOutcome::Error {
                                            tool_call_id: tc.id.clone(),
                                            tool_name: tc.name.clone(),
                                            error: result,
                                        });
                                        continue;
                                    }
                                    let mut approval_request = prepare_approval_request_for_tool(
                                        &tc,
                                        &state.thread_id.0,
                                        &state.run_id.0,
                                        state.config.metadata.as_ref(),
                                        approval_reviewer.as_deref(),
                                    )
                                    .await;
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
                                    let mut user_interrupted_by_approval_deny = false;
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
                                            let permission_hook = hook_manager
                                                .run_tool(
                                                    HookEventName::PermissionRequest,
                                                    &hook_context,
                                                    &tool_hook_context(&tc, None),
                                                )
                                                .await;
                                            if permission_hook.permission_decision == Some(HookPermissionDecision::Allow) {
                                                let (resolution, event) = self
                                                    .approval_manager
                                                    .finish_request(
                                                        &approval_request,
                                                        crate::approval::ToolApprovalDecision::AllowOnce,
                                                    )
                                                    .await;
                                                if let ApprovalEvent::Resolved { request, decision } = event {
                                                    yield CatEvent::ToolApprovalResolved { request, decision };
                                                }
                                                resolution
                                            } else if permission_hook.permission_decision == Some(HookPermissionDecision::Deny)
                                                || permission_hook.blocked
                                            {
                                                let (resolution, event) = self
                                                    .approval_manager
                                                    .finish_request(
                                                        &approval_request,
                                                        crate::approval::ToolApprovalDecision::Deny,
                                                    )
                                                    .await;
                                                if let ApprovalEvent::Resolved { request, decision } = event {
                                                    yield CatEvent::ToolApprovalResolved { request, decision };
                                                }
                                                resolution
                                            } else {
                                            let should_wait = matches!(
                                                approval_request.platform.as_deref(),
                                                Some("web" | "tui" | "feishu")
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
                                            let mut cancelled_while_waiting = false;
                                            let decision = if should_wait {
                                                if let Some(cancel) = cancel_signal.as_ref() {
                                                    tokio::select! {
                                                        decision = rx => decision.unwrap_or(crate::approval::ToolApprovalDecision::Deny),
                                                        _ = cancel.cancelled() => {
                                                            cancelled_while_waiting = true;
                                                            crate::approval::ToolApprovalDecision::Deny
                                                        }
                                                    }
                                                } else {
                                                    rx.await.unwrap_or(crate::approval::ToolApprovalDecision::Deny)
                                                }
                                            } else {
                                                crate::approval::ToolApprovalDecision::Deny
                                            };
                                            user_interrupted_by_approval_deny = should_wait
                                                && decision == crate::approval::ToolApprovalDecision::Deny;
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
                                            if cancelled_while_waiting {
                                                yield stats_event(stats);
                                                yield CatEvent::Cancelled;
                                                return;
                                            }
                                            resolution
                                            }
                                        }
                                    };
                                    if approval_resolution == ApprovalResolution::Approved {
                                        log_tool_call_started(
                                            "local",
                                            &state.thread_id.0,
                                            &state.run_id.0,
                                            &tc,
                                        );
                                        yield CatEvent::ToolCall {
                                            id: tc.id.clone(),
                                            name: tc.name.clone(),
                                            args: tc.arguments.clone(),
                                        };
                                        approved_local.push(tc.clone());
                                    } else {
                                        tracing::info!(
                                            thread_id = %state.thread_id.0,
                                            run_id = %state.run_id.0,
                                            tool_call_id = %tc.id,
                                            tool_name = %tc.name,
                                            "tool_approval.denied_abort"
                                        );
                                        yield stats_event(stats);
                                        if user_interrupted_by_approval_deny {
                                            yield CatEvent::UserInterrupted {
                                                reason: "tool approval denied by user".to_string(),
                                            };
                                        }
                                        yield CatEvent::Done;
                                        return;
                                    }
                                }

                                let resume_map = HashMap::new();
                                let tool_names: HashMap<String, String> = approved_local
                                    .iter()
                                    .map(|tc| (tc.id.clone(), tc.name.clone()))
                                    .collect();
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
                                let execute_fut = execute_runtime_tools(
                                    &self.local_tools,
                                    self.model_tools.as_ref(),
                                    &approved_local,
                                    &resume_map,
                                    &tool_ctx,
                                );
                                tokio::pin!(execute_fut);
                                let results = if let Some(cancel) = cancel_signal.as_ref() {
                                    tokio::select! {
                                        _ = cancel.cancelled() => {
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                tool_kind = "local",
                                                approved_count = approved_local.len(),
                                                elapsed_ms = batch_started.elapsed().as_millis() as u64,
                                                "tool_batch.cancelled"
                                            );
                                            state.user_state = tool_ctx.user_state();
                                            complete_interrupted_tool_results(
                                                &mut state,
                                                &approved_local,
                                                &mut all_outcomes,
                                            );
                                            yield CatEvent::History(
                                                state.messages.clone(),
                                                state.user_state.clone(),
                                            );
                                            yield CatEvent::Cancelled;
                                            return;
                                        }
                                        results = &mut execute_fut => results,
                                    }
                                } else {
                                    execute_fut.await
                                };
                                tracing::info!(
                                    thread_id = %state.thread_id.0,
                                    run_id = %state.run_id.0,
                                    tool_kind = "local",
                                    approved_count = approved_local.len(),
                                    elapsed_ms = batch_started.elapsed().as_millis() as u64,
                                    "tool_batch.execute_returned"
                                );

                                let (side_tx, mut side_rx) = mpsc::unbounded_channel();
                                let task_thread_id = outer_thread_id_from_metadata(
                                    &state.thread_id.0,
                                    state.config.metadata.as_ref(),
                                );
                                let mut pending_results = collect_tool_result_futures_with_timeout(
                                    results,
                                    &approved_local,
                                    &tool_names,
                                    &data_dir,
                                    overflow_bytes,
                                    Some(side_tx),
                                    Arc::clone(&self.tool_tasks),
                                    task_thread_id.clone(),
                                    state.run_id.0.clone(),
                                    tool_foreground_timeout(),
                                    options.async_agent,
                                );
                                let mut completed_contents: HashMap<String, Content> = HashMap::new();
                                let mut forward_background_side_events = false;
                                let mut approval_pending = false;
                                while !pending_results.is_empty() {
                                    tokio::select! {
                                        Some(side_event) = side_rx.recv() => {
                                            let denied = is_denied_approval_event(&side_event);
                                            if is_approval_request_event(&side_event) {
                                                approval_pending = true;
                                            } else if is_approval_resolved_event(&side_event) {
                                                approval_pending = false;
                                            }
                                            yield side_event;
                                            if denied {
                                                tracing::info!(
                                                    thread_id = %state.thread_id.0,
                                                    run_id = %state.run_id.0,
                                                    tool_kind = "local",
                                                    "tool_approval.denied_side_event_abort"
                                                );
                                                yield stats_event(stats);
                                                yield CatEvent::Done;
                                                return;
                                            }
                                        }
                                        Some(outcome) = pending_results.next() => {
                                            let (call_id, mut collected, timed_out_task_id) = match outcome {
                                                ForegroundToolOutcome::TimedOut { .. } if approval_pending => {
                                                    // A human-facing interrupt (approval / user
                                                    // question) is pending – do not move the tool to
                                                    // the background while waiting for a response.
                                                    continue;
                                                }
                                                ForegroundToolOutcome::Completed { call_id, collected } => (call_id, collected, None),
                                                ForegroundToolOutcome::TimedOut { call_id, task_id } => {
                                                    let tc = approved_local.iter().find(|t| t.id == call_id).unwrap();
                                                    forward_background_side_events = options.async_agent;
                                                    let collected = if options.async_agent {
                                                        background_task_tool_result(&task_id, &tc.name)
                                                    } else {
                                                        manual_task_tool_result(&task_id, &tc.name)
                                                    };
                                                    (call_id, collected, Some(task_id))
                                                }
                                            };
                                            let tc = approved_local.iter().find(|t| t.id == call_id).unwrap();
                                            let elapsed_ms = started_at
                                                .get(&call_id)
                                                .map(|instant| instant.elapsed().as_millis() as u64)
                                                .unwrap_or(0);
                                            let hook_context = hook_context_from_state(
                                                &state,
                                                &workspace_root,
                                            );
                                            let post_hook = if timed_out_task_id.is_none() {
                                                hook_manager
                                                .run_tool(
                                                    HookEventName::PostToolUse,
                                                    &hook_context,
                                                    &tool_hook_context(
                                                        tc,
                                                        Some(serde_json::Value::String(collected.preview.clone())),
                                                    ),
                                                )
                                                .await
                                            } else {
                                                Default::default()
                                            };
                                            if post_hook.blocked {
                                                let blocked = post_hook
                                                    .reason
                                                    .unwrap_or_else(|| "error: tool result blocked by hook".to_string());
                                                collected = CollectedToolResult::text(blocked);
                                            }
                                            let success = crate::tool_pretty::tool_success(&collected.preview) && !post_hook.blocked;
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
                                            yield stats_event(stats);

                                            for side_ev in make_side_events(tc, &collected.preview) {
                                                yield side_ev;
                                            }
                                            for side_ev in collected.side_events.clone() {
                                                yield side_ev;
                                            }

                                            completed_contents.insert(call_id, collected.content);
                                        }
                                        _ = async {
                                            if let Some(cancel) = cancel_signal.as_ref() {
                                                cancel.cancelled().await;
                                            } else {
                                                std::future::pending::<()>().await;
                                            }
                                        } => {
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                tool_kind = "local",
                                                "tool_result_collection.cancelled"
                                            );
                                            push_completed_tool_contents(
                                                &approved_local,
                                                &mut completed_contents,
                                                &mut all_outcomes,
                                            );
                                            state.user_state = tool_ctx.user_state();
                                            complete_interrupted_tool_results(
                                                &mut state,
                                                &approved_local,
                                                &mut all_outcomes,
                                            );
                                            yield CatEvent::History(
                                                state.messages.clone(),
                                                state.user_state.clone(),
                                            );
                                            yield CatEvent::Cancelled;
                                            return;
                                        }
                                    }
                                }
                                if forward_background_side_events {
                                    spawn_background_side_event_forwarder(
                                        task_thread_id,
                                        Arc::clone(&self.tool_tasks),
                                        side_rx,
                                    );
                                }
                                push_completed_tool_contents(
                                    &approved_local,
                                    &mut completed_contents,
                                    &mut all_outcomes,
                                );
                                state.user_state = tool_ctx.user_state();
                                yield CatEvent::StateUpdate(state.user_state.clone());
                            }

                            if !dynamic.is_empty() {
                                let mut approved_dynamic = Vec::new();
                                for original_tc in &dynamic {
                                    let mut tc = original_tc.clone();
                                    let hook_context = hook_context_from_state(
                                        &state,
                                        &workspace_root,
                                    );
                                    let pre_hook = hook_manager
                                        .run_tool(
                                            HookEventName::PreToolUse,
                                            &hook_context,
                                            &tool_hook_context(&tc, None),
                                        )
                                        .await;
                                    if let Some(updated_input) = pre_hook.updated_input {
                                        tc.arguments = remi_tool_input_from_hook_update(
                                            &tc.name,
                                            &tc.arguments,
                                            updated_input,
                                        );
                                    }
                                    if pre_hook.blocked || pre_hook.permission_decision == Some(HookPermissionDecision::Deny) {
                                        let result = pre_hook
                                            .reason
                                            .unwrap_or_else(|| "error: tool execution blocked by hook".to_string());
                                        yield CatEvent::ToolCallResult {
                                            id: tc.id.clone(),
                                            name: tc.name.clone(),
                                            args: tc.arguments.clone(),
                                            result: result.clone(),
                                            success: false,
                                            elapsed_ms: 0,
                                        };
                                        yield stats_event(stats);
                                        all_outcomes.push(ToolCallOutcome::Error {
                                            tool_call_id: tc.id.clone(),
                                            tool_name: tc.name.clone(),
                                            error: result,
                                        });
                                        continue;
                                    }
                                    let mut approval_request = prepare_approval_request_for_tool(
                                        &tc,
                                        &state.thread_id.0,
                                        &state.run_id.0,
                                        state.config.metadata.as_ref(),
                                        approval_reviewer.as_deref(),
                                    )
                                    .await;
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
                                    let mut user_interrupted_by_approval_deny = false;
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
                                            let permission_hook = hook_manager
                                                .run_tool(
                                                    HookEventName::PermissionRequest,
                                                    &hook_context,
                                                    &tool_hook_context(&tc, None),
                                                )
                                                .await;
                                            if permission_hook.permission_decision == Some(HookPermissionDecision::Allow) {
                                                let (resolution, event) = self
                                                    .approval_manager
                                                    .finish_request(
                                                        &approval_request,
                                                        crate::approval::ToolApprovalDecision::AllowOnce,
                                                    )
                                                    .await;
                                                if let ApprovalEvent::Resolved { request, decision } = event {
                                                    yield CatEvent::ToolApprovalResolved { request, decision };
                                                }
                                                resolution
                                            } else if permission_hook.permission_decision == Some(HookPermissionDecision::Deny)
                                                || permission_hook.blocked
                                            {
                                                let (resolution, event) = self
                                                    .approval_manager
                                                    .finish_request(
                                                        &approval_request,
                                                        crate::approval::ToolApprovalDecision::Deny,
                                                    )
                                                    .await;
                                                if let ApprovalEvent::Resolved { request, decision } = event {
                                                    yield CatEvent::ToolApprovalResolved { request, decision };
                                                }
                                                resolution
                                            } else {
                                            let should_wait = matches!(
                                                approval_request.platform.as_deref(),
                                                Some("web" | "tui" | "feishu")
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
                                            let mut cancelled_while_waiting = false;
                                            let decision = if should_wait {
                                                if let Some(cancel) = cancel_signal.as_ref() {
                                                    tokio::select! {
                                                        decision = rx => decision.unwrap_or(crate::approval::ToolApprovalDecision::Deny),
                                                        _ = cancel.cancelled() => {
                                                            cancelled_while_waiting = true;
                                                            crate::approval::ToolApprovalDecision::Deny
                                                        }
                                                    }
                                                } else {
                                                    rx.await.unwrap_or(crate::approval::ToolApprovalDecision::Deny)
                                                }
                                            } else {
                                                crate::approval::ToolApprovalDecision::Deny
                                            };
                                            user_interrupted_by_approval_deny = should_wait
                                                && decision == crate::approval::ToolApprovalDecision::Deny;
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
                                            if cancelled_while_waiting {
                                                yield stats_event(stats);
                                                yield CatEvent::Cancelled;
                                                return;
                                            }
                                            resolution
                                            }
                                        }
                                    };
                                    if approval_resolution == ApprovalResolution::Approved {
                                        log_tool_call_started(
                                            "dynamic",
                                            &state.thread_id.0,
                                            &state.run_id.0,
                                            &tc,
                                        );
                                        yield CatEvent::ToolCall {
                                            id: tc.id.clone(),
                                            name: tc.name.clone(),
                                            args: tc.arguments.clone(),
                                        };
                                        approved_dynamic.push(tc.clone());
                                    } else {
                                        tracing::info!(
                                            thread_id = %state.thread_id.0,
                                            run_id = %state.run_id.0,
                                            tool_call_id = %tc.id,
                                            tool_name = %tc.name,
                                            "tool_approval.denied_abort"
                                        );
                                        yield stats_event(stats);
                                        if user_interrupted_by_approval_deny {
                                            yield CatEvent::UserInterrupted {
                                                reason: "tool approval denied by user".to_string(),
                                            };
                                        }
                                        yield CatEvent::Done;
                                        return;
                                    }
                                }

                                let resume_map = HashMap::new();
                                let tool_names: HashMap<String, String> = approved_dynamic
                                    .iter()
                                    .map(|tc| (tc.id.clone(), tc.name.clone()))
                                    .collect();
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
                                let execute_fut =
                                    dynamic_tools.execute_parallel(&approved_dynamic, &resume_map, &tool_ctx);
                                tokio::pin!(execute_fut);
                                let results = if let Some(cancel) = cancel_signal.as_ref() {
                                    tokio::select! {
                                        _ = cancel.cancelled() => {
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                tool_kind = "dynamic",
                                                approved_count = approved_dynamic.len(),
                                                elapsed_ms = batch_started.elapsed().as_millis() as u64,
                                                "tool_batch.cancelled"
                                            );
                                            state.user_state = tool_ctx.user_state();
                                            complete_interrupted_tool_results(
                                                &mut state,
                                                &approved_dynamic,
                                                &mut all_outcomes,
                                            );
                                            yield CatEvent::History(
                                                state.messages.clone(),
                                                state.user_state.clone(),
                                            );
                                            yield CatEvent::Cancelled;
                                            return;
                                        }
                                        results = &mut execute_fut => results,
                                    }
                                } else {
                                    execute_fut.await
                                };
                                tracing::info!(
                                    thread_id = %state.thread_id.0,
                                    run_id = %state.run_id.0,
                                    tool_kind = "dynamic",
                                    approved_count = approved_dynamic.len(),
                                    elapsed_ms = batch_started.elapsed().as_millis() as u64,
                                    "tool_batch.execute_returned"
                                );

                                let (side_tx, mut side_rx) = mpsc::unbounded_channel();
                                let task_thread_id = outer_thread_id_from_metadata(
                                    &state.thread_id.0,
                                    state.config.metadata.as_ref(),
                                );
                                let mut pending_results = collect_tool_result_futures_with_timeout(
                                    results,
                                    &approved_dynamic,
                                    &tool_names,
                                    &data_dir,
                                    overflow_bytes,
                                    Some(side_tx),
                                    Arc::clone(&self.tool_tasks),
                                    task_thread_id.clone(),
                                    state.run_id.0.clone(),
                                    tool_foreground_timeout(),
                                    options.async_agent,
                                );
                                let mut completed_contents: HashMap<String, Content> = HashMap::new();
                                let mut forward_background_side_events = false;
                                let mut approval_pending = false;
                                while !pending_results.is_empty() {
                                    tokio::select! {
                                        Some(side_event) = side_rx.recv() => {
                                            let denied = is_denied_approval_event(&side_event);
                                            if is_approval_request_event(&side_event) {
                                                approval_pending = true;
                                            } else if is_approval_resolved_event(&side_event) {
                                                approval_pending = false;
                                            }
                                            yield side_event;
                                            if denied {
                                                tracing::info!(
                                                    thread_id = %state.thread_id.0,
                                                    run_id = %state.run_id.0,
                                                    tool_kind = "dynamic",
                                                    "tool_approval.denied_side_event_abort"
                                                );
                                                yield stats_event(stats);
                                                yield CatEvent::Done;
                                                return;
                                            }
                                        }
                                        Some(outcome) = pending_results.next() => {
                                            let (call_id, mut collected, timed_out_task_id) = match outcome {
                                                ForegroundToolOutcome::TimedOut { .. } if approval_pending => {
                                                    // A human-facing interrupt (approval / user
                                                    // question) is pending – do not move the tool to
                                                    // the background while waiting for a response.
                                                    continue;
                                                }
                                                ForegroundToolOutcome::Completed { call_id, collected } => (call_id, collected, None),
                                                ForegroundToolOutcome::TimedOut { call_id, task_id } => {
                                                    let tc = approved_dynamic.iter().find(|t| t.id == call_id).unwrap();
                                                    forward_background_side_events = options.async_agent;
                                                    let collected = if options.async_agent {
                                                        background_task_tool_result(&task_id, &tc.name)
                                                    } else {
                                                        manual_task_tool_result(&task_id, &tc.name)
                                                    };
                                                    (call_id, collected, Some(task_id))
                                                }
                                            };
                                            let tc = approved_dynamic.iter().find(|t| t.id == call_id).unwrap();
                                            let elapsed_ms = started_at
                                                .get(&call_id)
                                                .map(|instant| instant.elapsed().as_millis() as u64)
                                                .unwrap_or(0);
                                            let hook_context = hook_context_from_state(
                                                &state,
                                                &workspace_root,
                                            );
                                            let post_hook = if timed_out_task_id.is_none() {
                                                hook_manager
                                                .run_tool(
                                                    HookEventName::PostToolUse,
                                                    &hook_context,
                                                    &tool_hook_context(
                                                        tc,
                                                        Some(serde_json::Value::String(collected.preview.clone())),
                                                    ),
                                                )
                                                .await
                                            } else {
                                                Default::default()
                                            };
                                            if post_hook.blocked {
                                                let blocked = post_hook
                                                    .reason
                                                    .unwrap_or_else(|| "error: tool result blocked by hook".to_string());
                                                collected = CollectedToolResult::text(blocked);
                                            }
                                            let success = crate::tool_pretty::tool_success(&collected.preview) && !post_hook.blocked;
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
                                            yield stats_event(stats);
                                            for side_ev in collected.side_events.clone() {
                                                yield side_ev;
                                            }

                                            completed_contents.insert(call_id, collected.content);
                                        }
                                        _ = async {
                                            if let Some(cancel) = cancel_signal.as_ref() {
                                                cancel.cancelled().await;
                                            } else {
                                                std::future::pending::<()>().await;
                                            }
                                        } => {
                                            tracing::info!(
                                                thread_id = %state.thread_id.0,
                                                run_id = %state.run_id.0,
                                                tool_kind = "dynamic",
                                                "tool_result_collection.cancelled"
                                            );
                                            push_completed_tool_contents(
                                                &approved_dynamic,
                                                &mut completed_contents,
                                                &mut all_outcomes,
                                            );
                                            state.user_state = tool_ctx.user_state();
                                            complete_interrupted_tool_results(
                                                &mut state,
                                                &approved_dynamic,
                                                &mut all_outcomes,
                                            );
                                            yield CatEvent::History(
                                                state.messages.clone(),
                                                state.user_state.clone(),
                                            );
                                            yield CatEvent::Cancelled;
                                            return;
                                        }
                                    }
                                }
                                if forward_background_side_events {
                                    spawn_background_side_event_forwarder(
                                        task_thread_id,
                                        Arc::clone(&self.tool_tasks),
                                        side_rx,
                                    );
                                }
                                push_completed_tool_contents(
                                    &approved_dynamic,
                                    &mut completed_contents,
                                    &mut all_outcomes,
                                );
                            }

                            if !unavailable.is_empty() {
                                let names = unavailable
                                    .iter()
                                    .map(|(t, _)| t.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                tracing::warn!(
                                    thread_id = %state.thread_id.0,
                                    run_id = %state.run_id.0,
                                    tool_names = %names,
                                    tool_count = unavailable.len(),
                                    "tool_call.failed"
                                );
                                for (tc, reason) in &unavailable {
                                    let result = reason.error_message(&tc.name);
                                    log_tool_call_started(
                                        reason.tool_kind(),
                                        &state.thread_id.0,
                                        &state.run_id.0,
                                        tc,
                                    );
                                    yield CatEvent::ToolCall {
                                        id: tc.id.clone(),
                                        name: tc.name.clone(),
                                        args: tc.arguments.clone(),
                                    };
                                    yield CatEvent::ToolCallResult {
                                        id: tc.id.clone(),
                                        name: tc.name.clone(),
                                        args: tc.arguments.clone(),
                                        result: result.clone(),
                                        success: false,
                                        elapsed_ms: 0,
                                    };
                                    yield stats_event(stats);
                                    all_outcomes.push(ToolCallOutcome::Error {
                                        tool_call_id: tc.id.clone(),
                                        tool_name: tc.name.clone(),
                                        error: result,
                                    });
                                }
                            }

                            refresh_runtime_tool_definitions(
                                &mut state,
                                &self.local_tools,
                                self.model_tools.as_ref(),
                                &dynamic_tools,
                                tool_allowlist.as_deref(),
                            );

                            if let Some(steer) = options.steer.as_ref() {
                                if let Some(batch) = steer.drain_batch() {
                                    yield CatEvent::SteerInjected(crate::SteerInjectedEvent {
                                        steer_ids: batch.ids.clone(),
                                        session_id: state.thread_id.0.clone(),
                                        preview: batch.preview.clone(),
                                        count: batch.count,
                                    });
                                    next_input =
                                        Some(steer_start_input_after_tool_results(state, all_outcomes, batch));
                                    break;
                                }
                            }

                            next_input = Some(LoopInput::Resume {
                                state,
                                pending_interrupts: Vec::new(),
                                results: all_outcomes,
                            });
                            break;
                        }
                        CoreDriveEvent::Checkpoint { state } => {
                            if let Some(steer) = options.steer.as_ref() {
                                if let Some(batch) = steer.drain_batch() {
                                    yield CatEvent::SteerInjected(crate::SteerInjectedEvent {
                                        steer_ids: batch.ids.clone(),
                                        session_id: state.thread_id.0.clone(),
                                        preview: batch.preview.clone(),
                                        count: batch.count,
                                    });
                                    next_input = Some(steer_start_input(state, batch));
                                    break;
                                }
                            }
                            last_messages = state.messages.clone();
                            last_user_state = state.user_state.clone();
                            last_checkpoint_state = Some(state);
                        }
                        CoreDriveEvent::Done {
                            stats,
                            tool_rounds,
                            ..
                        } => {
                            tracing::info!(
                                thread_id = %log_thread_id,
                                run_id = %log_run_id,
                                supervisor_run,
                                tool_rounds,
                                prompt_tokens = stats.prompt_tokens,
                                completion_tokens = stats.completion_tokens,
                                max_prompt_tokens = stats.max_prompt_tokens,
                                elapsed_ms = stats.elapsed_ms,
                                "agent_run.completed"
                            );
                            yield stats_event(stats);
                            yield CatEvent::History(last_messages.clone(), last_user_state.clone());
                            yield CatEvent::Done;
                            return;
                        }
                        CoreDriveEvent::Error {
                            error: e,
                            stats,
                            tool_rounds,
                            ..
                        } => {
                            tracing::warn!(
                                thread_id = %log_thread_id,
                                run_id = %log_run_id,
                                supervisor_run,
                                tool_rounds,
                                prompt_tokens = stats.prompt_tokens,
                                completion_tokens = stats.completion_tokens,
                                max_prompt_tokens = stats.max_prompt_tokens,
                                elapsed_ms = stats.elapsed_ms,
                                error = %e,
                                "agent_run.failed"
                            );
                            yield CatEvent::Error(e);
                            return;
                        }
                        CoreDriveEvent::Cancelled {
                            kind,
                            state,
                            stats,
                            tool_rounds,
                            ..
                        } => {
                            tracing::info!(
                                thread_id = %log_thread_id,
                                run_id = %log_run_id,
                                supervisor_run,
                                tool_rounds,
                                elapsed_ms = stats.elapsed_ms,
                                "agent_run.cancelled"
                            );
                            if let Some(mut state) = state {
                                complete_pending_tool_calls_in_state(&mut state);
                                yield CatEvent::History(
                                    state.messages.clone(),
                                    state.user_state.clone(),
                                );
                                if kind == CoreCancelKind::Signal {
                                    yield CatEvent::Cancelled;
                                } else {
                                    yield CatEvent::Done;
                                }
                                return;
                            }
                            if kind == CoreCancelKind::Signal {
                                yield CatEvent::Cancelled;
                                return;
                            }
                            yield CatEvent::History(last_messages.clone(), last_user_state.clone());
                            yield CatEvent::Done;
                            return;
                        }
                        CoreDriveEvent::ToolResult { .. }
                        | CoreDriveEvent::SubSession(_)
                        | CoreDriveEvent::ToolDelta { .. }
                        | CoreDriveEvent::Ignored => {}
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

fn steer_start_input(mut state: AgentState, batch: CoreSteerBatch) -> LoopInput {
    complete_pending_tool_calls_in_state(&mut state);
    let mut input = LoopInput::start_content(batch.content)
        .history(state.messages)
        .user_state(state.user_state);
    if let Some(metadata) = state.config.metadata {
        input = input.metadata(metadata);
    }
    if let Some(message_metadata) = batch.message_metadata {
        input = input.message_metadata(message_metadata);
    }
    if let Some(user_name) = batch.user_name {
        input = input.user_name(user_name);
    }
    input
}

fn steer_start_input_after_tool_results(
    mut state: AgentState,
    results: Vec<ToolCallOutcome>,
    batch: CoreSteerBatch,
) -> LoopInput {
    append_tool_results_to_history(&mut state.messages, results);
    complete_pending_tool_calls_in_state(&mut state);
    steer_start_input(state, batch)
}

fn append_tool_results_to_history(messages: &mut Vec<Message>, results: Vec<ToolCallOutcome>) {
    let mut materialized = messages
        .iter()
        .filter_map(|message| message.tool_call_id.clone())
        .collect::<HashSet<_>>();
    for result in results {
        match result {
            ToolCallOutcome::Result {
                tool_call_id,
                content,
                ..
            } => {
                if materialized.insert(tool_call_id.clone()) {
                    messages.push(Message::tool_result_content(tool_call_id, content));
                }
            }
            ToolCallOutcome::Error {
                tool_call_id,
                error,
                ..
            } => {
                if materialized.insert(tool_call_id.clone()) {
                    messages.push(Message::tool_result(
                        tool_call_id,
                        format!("error: {error}"),
                    ));
                }
            }
        }
    }
}

fn tool_outcome_call_ids(results: &[ToolCallOutcome]) -> HashSet<String> {
    results
        .iter()
        .map(|result| match result {
            ToolCallOutcome::Result { tool_call_id, .. }
            | ToolCallOutcome::Error { tool_call_id, .. } => tool_call_id.clone(),
        })
        .collect()
}

fn message_tool_result_call_ids(messages: &[Message]) -> HashSet<String> {
    messages
        .iter()
        .filter_map(|message| message.tool_call_id.clone())
        .collect()
}

fn push_completed_tool_contents(
    tool_calls: &[ParsedToolCall],
    completed_contents: &mut HashMap<String, Content>,
    outcomes: &mut Vec<ToolCallOutcome>,
) {
    let mut materialized = tool_outcome_call_ids(outcomes);
    for tc in tool_calls {
        if !materialized.insert(tc.id.clone()) {
            completed_contents.remove(&tc.id);
            continue;
        }
        if let Some(content) = completed_contents.remove(&tc.id) {
            outcomes.push(ToolCallOutcome::Result {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                content,
            });
        }
    }
}

fn complete_interrupted_tool_results(
    state: &mut AgentState,
    tool_calls: &[ParsedToolCall],
    outcomes: &mut Vec<ToolCallOutcome>,
) {
    let mut materialized = message_tool_result_call_ids(&state.messages);
    materialized.extend(tool_outcome_call_ids(outcomes));

    for tc in tool_calls {
        if materialized.insert(tc.id.clone()) {
            outcomes.push(ToolCallOutcome::Error {
                tool_call_id: tc.id.clone(),
                tool_name: tc.name.clone(),
                error: INTERRUPTED_TOOL_RESULT_ERROR.to_string(),
            });
        }
    }
    let outcomes = std::mem::take(outcomes);
    append_tool_results_to_history(&mut state.messages, outcomes);
}

fn complete_pending_tool_calls_in_state(state: &mut AgentState) {
    let mut repaired = Vec::with_capacity(state.messages.len());
    let mut pending = Vec::<(String, String)>::new();

    for message in std::mem::take(&mut state.messages) {
        if !pending.is_empty() && message.tool_call_id.is_none() {
            append_interrupted_tool_results(&mut repaired, std::mem::take(&mut pending));
        }

        if let Some(tool_call_id) = message.tool_call_id.as_deref() {
            pending.retain(|(id, _)| id != tool_call_id);
        }

        let tool_calls = message.tool_calls.clone();
        repaired.push(message);

        if let Some(calls) = tool_calls {
            let materialized = message_tool_result_call_ids(&repaired);
            for call in calls {
                if !materialized.contains(&call.id) && !pending.iter().any(|(id, _)| id == &call.id)
                {
                    pending.push((call.id, call.function.name));
                }
            }
        }
    }

    append_interrupted_tool_results(&mut repaired, pending);
    state.messages = repaired;
}

fn append_interrupted_tool_results(messages: &mut Vec<Message>, pending: Vec<(String, String)>) {
    let mut materialized = message_tool_result_call_ids(messages);
    for (tool_call_id, _tool_name) in pending {
        if materialized.insert(tool_call_id.clone()) {
            messages.push(Message::tool_result(
                tool_call_id,
                format!("error: {INTERRUPTED_TOOL_RESULT_ERROR}"),
            ));
        }
    }
}

fn merge_runtime_tool_definitions(
    local_tools: &DefaultToolRegistry,
    model_tools: Option<&DefaultToolRegistry>,
    dynamic_tools: &DefaultToolRegistry,
    ctx: &ToolDefinitionContext,
    external_defs: &[ToolDefinition],
    tool_allowlist: Option<&[String]>,
) -> Vec<ToolDefinition> {
    let mut defs = local_tools.definitions_with_context(ctx);
    if let Some(model_tools) = model_tools {
        defs.extend(model_tools.definitions_with_context(ctx));
    }
    defs.extend(dynamic_tools.definitions_with_context(ctx));
    defs.extend_from_slice(external_defs);
    filter_tool_allowlist(tool_allowlist, defs)
}

fn refresh_runtime_tool_definitions(
    state: &mut remi_agentloop::prelude::AgentState,
    local_tools: &DefaultToolRegistry,
    model_tools: Option<&DefaultToolRegistry>,
    dynamic_tools: &DefaultToolRegistry,
    tool_allowlist: Option<&[String]>,
) {
    let external_defs: Vec<_> = state
        .tool_definitions
        .iter()
        .filter(|definition| {
            let name = definition.function.name.as_str();
            !runtime_tools_contains(local_tools, model_tools, name) && !dynamic_tools.contains(name)
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
        model_tools,
        dynamic_tools,
        &ctx,
        &external_defs,
        tool_allowlist,
    );
}

fn runtime_tools_contains(
    local_tools: &DefaultToolRegistry,
    model_tools: Option<&DefaultToolRegistry>,
    name: &str,
) -> bool {
    local_tools.contains(name)
        || model_tools
            .map(|tools| tools.contains(name))
            .unwrap_or(false)
}

async fn execute_runtime_tools<'a>(
    local_tools: &'a DefaultToolRegistry,
    model_tools: Option<&'a DefaultToolRegistry>,
    calls: &'a [ParsedToolCall],
    resume_map: &'a HashMap<String, ResumePayload>,
    ctx: &'a ToolContext,
) -> Vec<(String, Result<BoxedToolResult, AgentError>)> {
    let Some(model_tools) = model_tools else {
        return local_tools.execute_parallel(calls, resume_map, ctx).await;
    };
    if model_tools.is_empty() {
        return local_tools.execute_parallel(calls, resume_map, ctx).await;
    }

    let local_results = local_tools.execute_parallel(calls, resume_map, ctx).await;
    let model_results = model_tools.execute_parallel(calls, resume_map, ctx).await;
    let mut local_by_id = local_results
        .into_iter()
        .collect::<HashMap<String, Result<BoxedToolResult, AgentError>>>();
    let mut model_by_id = model_results
        .into_iter()
        .collect::<HashMap<String, Result<BoxedToolResult, AgentError>>>();
    let mut results = Vec::with_capacity(calls.len());
    for call in calls {
        let result = if model_tools.contains(&call.name) {
            model_by_id.remove(&call.id)
        } else {
            local_by_id.remove(&call.id)
        }
        .unwrap_or_else(|| Err(AgentError::ToolNotFound(call.name.clone())));
        results.push((call.id.clone(), result));
    }
    results
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

fn hook_context_from_state(state: &remi_agentloop::prelude::AgentState, cwd: &Path) -> HookContext {
    HookContext {
        session_id: state
            .config
            .metadata
            .as_ref()
            .and_then(|value| value.get("thread_id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&state.thread_id.0)
            .to_string(),
        transcript_path: None,
        cwd: cwd.to_path_buf(),
        model: None,
        turn_id: Some(state.run_id.0.clone()),
        permission_mode: None,
    }
}

fn tool_hook_context(
    tc: &ParsedToolCall,
    tool_response: Option<serde_json::Value>,
) -> ToolHookContext {
    ToolHookContext {
        tool_name: tc.name.clone(),
        tool_use_id: tc.id.clone(),
        tool_input: tc.arguments.clone(),
        tool_response,
    }
}

fn approval_request_for_tool(
    tc: &ParsedToolCall,
    session_id: &str,
    run_id: &str,
    metadata: Option<&serde_json::Value>,
) -> ToolApprovalRequest {
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
        risk: ToolRiskLevel::Medium,
        args_summary: summarize_tool_args(&tc.arguments),
        command_key: Some(command_key(&tc.name, &tc.arguments)),
        model_review_reason: None,
        platform: metadata
            .and_then(|value| value.get("platform"))
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        review: None,
    }
}

fn outer_thread_id_from_metadata(
    fallback_thread_id: &str,
    metadata: Option<&serde_json::Value>,
) -> String {
    metadata
        .and_then(|value| value.get("thread_id"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_thread_id)
        .to_string()
}

async fn prepare_approval_request_for_tool(
    tc: &ParsedToolCall,
    session_id: &str,
    run_id: &str,
    metadata: Option<&serde_json::Value>,
    reviewer: Option<&ModelApprovalReviewer>,
) -> ToolApprovalRequest {
    let mut request = approval_request_for_tool(tc, session_id, run_id, metadata);
    if crate::metadata_flag_enabled(metadata, "supervisor_run") {
        request.risk = ToolRiskLevel::Low;
        request.review = Some(review_tool_risk(
            &request.tool_name,
            &request.args_summary,
            request.risk,
        ));
        return request;
    }
    match classify_tool_risk_assessment(&tc.name, &tc.arguments) {
        ToolRiskAssessment::Known(risk) => {
            request.risk = risk;
            request.review = Some(review_tool_risk(
                &request.tool_name,
                &request.args_summary,
                request.risk,
            ));
            request
        }
        ToolRiskAssessment::NeedsModelReview { reason } => {
            request.model_review_reason = Some(reason);
            let review = match reviewer {
                Some(reviewer) => reviewer.review(&request).await,
                None => ToolRiskReview {
                    risk: ToolRiskLevel::High,
                    reason: "No approval model reviewer is configured.".to_string(),
                    concerns: vec!["Requires explicit human confirmation.".to_string()],
                },
            };
            request.risk = review.risk;
            request.review = Some(review);
            request
        }
    }
}

fn build_dynamic_tools(
    metadata: Option<serde_json::Value>,
    workspace_root: PathBuf,
    workspace_root_label: String,
    allow_host_absolute_paths: bool,
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
        register_im_tools(
            &mut registry,
            workspace_root,
            workspace_root_label,
            allow_host_absolute_paths,
            bridge,
        );
    }
    registry
}

/// Maximum byte length of a tool result returned inline.
/// Larger output is spilled to a temp file in `<data_dir>/tmp/`.
/// This const is the hard floor; the actual threshold comes from
/// [`CatAgent::overflow_bytes`] which is derived from the model profile.
const _OVERFLOW_THRESHOLD_DEFAULT: usize = 20_000;
const TOOL_OUTPUT_CHUNK_OVERHEAD_BYTES: usize = 512;
const DEFAULT_TOOL_FOREGROUND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn tool_foreground_timeout() -> std::time::Duration {
    std::env::var("REMI_TOOL_FOREGROUND_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(DEFAULT_TOOL_FOREGROUND_TIMEOUT)
}

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

enum ForegroundToolOutcome {
    Completed {
        call_id: String,
        collected: CollectedToolResult,
    },
    TimedOut {
        call_id: String,
        task_id: String,
    },
}

fn background_task_tool_result(task_id: &str, tool_name: &str) -> CollectedToolResult {
    CollectedToolResult::text(format!(
        "Tool `{tool_name}` is still running in the background.\n\
         task_id: {task_id}\n\
         Do not call the same tool again just to wait for this result, and do not continuously poll it. \
         The system will automatically send you the completed result and continue the session when it finishes. \
         You can work on unrelated user requests, other independent tasks, or patiently wait by doing nothing."
    ))
}

fn manual_task_tool_result(task_id: &str, tool_name: &str) -> CollectedToolResult {
    CollectedToolResult::text(format!(
        "Tool `{tool_name}` is still running in the background.\n\
         task_id: {task_id}\n\
         Use `tool_tasks` with action=get to inspect status/recent output, or action=cancel to cancel it."
    ))
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

#[cfg(test)]
async fn collect_tool_results_parallel(
    results: Vec<(String, Result<BoxedToolResult, AgentError>)>,
    tool_names: &HashMap<String, String>,
    data_dir: &Path,
    overflow_bytes: usize,
    side_event_tx: Option<mpsc::UnboundedSender<CatEvent>>,
) -> Vec<(String, CollectedToolResult)> {
    let data_dir = data_dir.to_path_buf();
    let futures = results.into_iter().map(|(call_id, result)| {
        let data_dir = data_dir.clone();
        let side_event_tx = side_event_tx.clone();
        let tool_name = tool_names.get(&call_id).cloned().unwrap_or_default();
        async move {
            let collected = collect_result_with_overflow(
                result,
                &data_dir,
                overflow_bytes,
                side_event_tx,
                Some(call_id.as_str()),
                tool_name.as_str(),
            )
            .await;
            (call_id, collected)
        }
    });
    futures::future::join_all(futures).await
}

fn collect_tool_result_futures_with_timeout(
    results: Vec<(String, Result<BoxedToolResult, AgentError>)>,
    calls: &[ParsedToolCall],
    tool_names: &HashMap<String, String>,
    data_dir: &Path,
    overflow_bytes: usize,
    side_event_tx: Option<mpsc::UnboundedSender<CatEvent>>,
    tool_tasks: Arc<crate::tool_tasks::ToolTaskManager>,
    thread_id: String,
    run_id: String,
    foreground_timeout: std::time::Duration,
    async_agent: bool,
) -> FuturesUnordered<LocalBoxFuture<'static, ForegroundToolOutcome>> {
    let data_dir = data_dir.to_path_buf();
    let call_args = calls
        .iter()
        .map(|call| (call.id.clone(), call.arguments.clone()))
        .collect::<HashMap<_, _>>();
    results
        .into_iter()
        .map(|(call_id, result)| {
            let data_dir = data_dir.clone();
            let side_event_tx = side_event_tx.clone();
            let tool_name = tool_names.get(&call_id).cloned().unwrap_or_default();
            let args = call_args
                .get(&call_id)
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let tool_tasks = Arc::clone(&tool_tasks);
            let thread_id = thread_id.clone();
            let run_id = run_id.clone();
            async move {
                let collect_call_id = call_id.clone();
                let collect_tool_name = tool_name.clone();
                let collect = async move {
                    let collected = collect_result_with_overflow(
                        result,
                        &data_dir,
                        overflow_bytes,
                        side_event_tx,
                        Some(collect_call_id.as_str()),
                        collect_tool_name.as_str(),
                    )
                    .await;
                    (collect_call_id, collected)
                };
                let cancel = CancellationToken::new();
                let task_id = tool_tasks
                    .start(
                        thread_id,
                        run_id,
                        call_id.clone(),
                        tool_name.clone(),
                        args,
                        cancel,
                    )
                    .await
                    .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
                let (completed_tx, mut completed_rx) =
                    oneshot::channel::<(String, CollectedToolResult)>();
                spawn_background_tool_result(
                    task_id.clone(),
                    tool_name,
                    Arc::clone(&tool_tasks),
                    collect.boxed_local(),
                    Some(completed_tx),
                );
                tokio::select! {
                    completed = &mut completed_rx => {
                        let (completed_call_id, collected) = completed.unwrap_or_else(|_| {
                            (
                                call_id.clone(),
                                CollectedToolResult::text("error: background tool task was cancelled"),
                            )
                        });
                        ForegroundToolOutcome::Completed {
                            call_id: completed_call_id,
                            collected,
                        }
                    }
                    _ = tokio::time::sleep(foreground_timeout) => {
                        if async_agent {
                            tool_tasks.enable_completion_notification(&task_id).await;
                        }
                        ForegroundToolOutcome::TimedOut {
                            call_id,
                            task_id,
                        }
                    }
                }
            }
            .boxed_local()
        })
        .collect()
}

fn spawn_background_tool_result(
    task_id: String,
    tool_name: String,
    tool_tasks: Arc<crate::tool_tasks::ToolTaskManager>,
    future: LocalBoxFuture<'static, (String, CollectedToolResult)>,
    completed_tx: Option<oneshot::Sender<(String, CollectedToolResult)>>,
) {
    let task_manager = Arc::clone(&tool_tasks);
    let task_id_for_attach = task_id.clone();
    let handle = tokio::task::spawn_local(async move {
        let started = Instant::now();
        let (call_id, collected) = future.await;
        let success = crate::tool_pretty::tool_success(&collected.preview);
        let elapsed_ms = started.elapsed().as_millis() as u64;
        for line in collected
            .preview
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            tool_tasks.append_output(&task_id, line.to_string()).await;
        }
        tracing::info!(
            task_id = %task_id,
            tool_name = %tool_name,
            success,
            elapsed_ms,
            "tool_task.completed"
        );
        let _ = tool_tasks
            .finish(&task_id, success, elapsed_ms, collected.preview.clone())
            .await;
        if let Some(completed_tx) = completed_tx {
            let _ = completed_tx.send((call_id, collected));
        }
    });
    tokio::task::spawn_local(async move {
        task_manager
            .attach_abort_handle(&task_id_for_attach, handle.abort_handle())
            .await;
        let _ = handle.await;
    });
}

fn spawn_background_side_event_forwarder(
    thread_id: String,
    tool_tasks: Arc<crate::tool_tasks::ToolTaskManager>,
    mut side_rx: mpsc::UnboundedReceiver<CatEvent>,
) {
    tokio::task::spawn_local(async move {
        while let Some(event) = side_rx.recv().await {
            tool_tasks.publish_side_event(thread_id.clone(), event);
        }
    });
}

async fn collect_result(
    result: Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
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
                    ToolOutput::Delta(delta) => {
                        let event = CatEvent::Text(delta);
                        if let Some(tx) = &side_event_tx {
                            let _ = tx.send(event);
                        } else {
                            side_events.push(event);
                        }
                    }
                    ToolOutput::SubSession(mut event) => {
                        if let Some(mut event) =
                            crate::cat_event_from_subagent_approval_marker(&event)
                        {
                            if let Some(parent) = parent_tool_call_id {
                                match &mut event {
                                    CatEvent::UserQuestionRequested(request)
                                    | CatEvent::UserQuestionUpdated(request) => {
                                        if request.tool_call_id.is_empty() {
                                            request.tool_call_id = parent.to_string();
                                        }
                                    }
                                    CatEvent::UserQuestionResolved { request, .. } => {
                                        if request.tool_call_id.is_empty() {
                                            request.tool_call_id = parent.to_string();
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if let Some(tx) = &side_event_tx {
                                let _ = tx.send(event);
                            } else {
                                side_events.push(event);
                            }
                            continue;
                        }
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
                    ToolOutput::Custom { .. } => {}
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
    result: Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
    data_dir: &Path,
    overflow_bytes: usize,
    side_event_tx: Option<mpsc::UnboundedSender<CatEvent>>,
    parent_tool_call_id: Option<&str>,
    tool_name: &str,
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

    let total = text.len();
    if tool_name == "fs_read" {
        let mut result = CollectedToolResult::text(fs_read_overflow_summary(total, overflow_bytes));
        result.side_events = side_events;
        return result;
    }
    let mut result = match spill_tool_output_chunks(data_dir, &text, overflow_bytes).await {
        Ok(spill) => CollectedToolResult::text(spill.summary(total)),
        Err(_) => CollectedToolResult::text(text),
    };
    result.side_events = side_events;
    result
}

fn fs_read_overflow_summary(total: usize, overflow_bytes: usize) -> String {
    let suggested = fs_read_safe_retry_length(overflow_bytes);
    format!(
        "[fs_read output too large ({total} bytes) — not saved to a temporary file]\n\
         Retry fs_read on the same path with a smaller length, for example length={suggested}, \
         and advance offset by the returned length until remaining=0."
    )
}

fn fs_read_safe_retry_length(overflow_bytes: usize) -> usize {
    overflow_bytes
        .saturating_sub(TOOL_OUTPUT_CHUNK_OVERHEAD_BYTES)
        .min(crate::tools::DEFAULT_FS_READ_LENGTH)
        .max(1)
}

#[derive(Debug, Clone)]
struct ToolOutputChunkSpill {
    spill_dir: PathBuf,
    part_count: usize,
    chunk_bytes: usize,
}

impl ToolOutputChunkSpill {
    fn summary(&self, total: usize) -> String {
        let width = part_number_width(self.part_count);
        let first = chunk_filename(1, self.part_count);
        let last = chunk_filename(self.part_count, self.part_count);
        let first_path = self.spill_dir.join(&first);
        let last_path = self.spill_dir.join(&last);
        format!(
            "[Output too large ({total} bytes) — split into {} chunk files under {}]\n\
             Each chunk is at most {} bytes; there is no complete single output file.\n\
             Read one chunk at a time with fs_read, for example path=\"{}\".\n\
             Continue in order through path=\"{}\". Do not concatenate or read the whole directory at once.\n\
             Filename pattern: part_<n>_of_<total>.txt with zero-padded {width}-digit numbers.",
            self.part_count,
            self.spill_dir.display(),
            self.chunk_bytes,
            first_path.display(),
            last_path.display()
        )
    }
}

async fn spill_tool_output_chunks(
    data_dir: &Path,
    text: &str,
    overflow_bytes: usize,
) -> std::io::Result<ToolOutputChunkSpill> {
    let chunk_bytes = tool_output_chunk_bytes(overflow_bytes);
    let chunks = split_utf8_chunks(text, chunk_bytes);
    let part_count = chunks.len().max(1);
    let dir_name = format!("tool_out_{}", uuid::Uuid::new_v4());
    let spill_dir = data_dir.join("tmp").join(&dir_name);
    tokio::fs::create_dir_all(&spill_dir).await?;

    for (index, chunk) in chunks.into_iter().enumerate() {
        let filename = chunk_filename(index + 1, part_count);
        tokio::fs::write(spill_dir.join(filename), chunk.as_bytes()).await?;
    }

    Ok(ToolOutputChunkSpill {
        spill_dir,
        part_count,
        chunk_bytes,
    })
}

fn tool_output_chunk_bytes(overflow_bytes: usize) -> usize {
    overflow_bytes
        .saturating_sub(TOOL_OUTPUT_CHUNK_OVERHEAD_BYTES)
        .min(crate::tools::DEFAULT_FS_READ_LENGTH)
        .max(1)
}

fn split_utf8_chunks(text: &str, max_bytes: usize) -> Vec<&str> {
    let max_bytes = max_bytes.max(1);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut current = 0usize;
    for (index, ch) in text.char_indices() {
        let next = index + ch.len_utf8();
        if current > start && next - start > max_bytes {
            chunks.push(&text[start..current]);
            start = index;
        }
        current = next;
    }
    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}

fn chunk_filename(index: usize, total: usize) -> String {
    let width = part_number_width(total);
    format!("part_{index:0width$}_of_{total:0width$}.txt")
}

fn part_number_width(total: usize) -> usize {
    total.max(1).to_string().len().max(4)
}

fn make_side_events(tc: &ParsedToolCall, result_str: &str) -> Vec<CatEvent> {
    let mut evs = Vec::new();
    if let Some(ev) = skill::make_skill_event(tc, result_str) {
        evs.push(CatEvent::Skill(ev));
    }
    for ev in todo::make_todo_events(tc, result_str) {
        evs.push(CatEvent::Todo(ev));
    }
    evs
}

#[cfg(test)]
mod tests {
    use super::{
        append_tool_results_to_history, approval_request_for_tool, background_task_tool_result,
        collect_result_with_overflow, collect_tool_result_futures_with_timeout,
        collect_tool_results_parallel, complete_interrupted_tool_results,
        complete_pending_tool_calls_in_state, fs_read_overflow_summary,
        outer_thread_id_from_metadata, spawn_background_side_event_forwarder, split_utf8_chunks,
        steer_start_input, steer_start_input_after_tool_results, tool_foreground_timeout,
        tool_output_chunk_bytes, CatAgent, ForegroundToolOutcome, INTERRUPTED_TOOL_RESULT_ERROR,
    };
    use crate::approval::ToolApprovalManager;
    use crate::events::CatEvent;
    use crate::user_question::UserQuestionManager;
    use bot_runtime_core::ToolContext;
    use bot_runtime_core::{
        CoreSteerBatch, CoreSteerInput, CoreSteerQueue, CoreSteerSource, CoreStreamOptions,
    };
    use futures::{stream, Stream, StreamExt};
    use remi_agentloop::prelude::{
        Agent, AgentError, AgentState, CancellationToken, Checkpoint, CheckpointStatus, Content,
        ContentPart, LoopInput, Message, ParsedToolCall, Role, StepConfig, ToolCallOutcome,
        ToolOutput, ToolResult,
    };
    use remi_agentloop::tool::{
        registry::DefaultToolRegistry, BoxedToolResult, BoxedToolStream, Tool,
    };
    use remi_agentloop::types::{FunctionCall, ToolCallMessage};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    use crate::hooks::HookManager;

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("remi-agent-result-test-{}", uuid::Uuid::new_v4()))
    }

    fn test_hook_manager() -> Arc<HookManager> {
        HookManager::new(test_root(), test_root())
    }

    fn test_tool_tasks() -> Arc<crate::tool_tasks::ToolTaskManager> {
        crate::tool_tasks::ToolTaskManager::load(test_root()).unwrap()
    }

    fn test_tool_call_message(id: &str, name: &str) -> ToolCallMessage {
        ToolCallMessage {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    fn test_parsed_tool_call(id: &str, name: &str) -> ParsedToolCall {
        ParsedToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        }
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

    #[test]
    fn background_task_uses_outer_thread_id_from_metadata() {
        assert_eq!(
            outer_thread_id_from_metadata(
                "agentloop-inner-thread",
                Some(&serde_json::json!({
                    "thread_id": "tui-session-1",
                    "platform": "tui"
                })),
            ),
            "tui-session-1"
        );
        assert_eq!(
            outer_thread_id_from_metadata(
                "agentloop-inner-thread",
                Some(&serde_json::json!({"thread_id": "   "})),
            ),
            "agentloop-inner-thread"
        );
    }

    #[test]
    fn tool_foreground_timeout_defaults_to_ten_seconds_and_env_overrides() {
        let previous = std::env::var("REMI_TOOL_FOREGROUND_TIMEOUT_MS").ok();
        unsafe {
            std::env::remove_var("REMI_TOOL_FOREGROUND_TIMEOUT_MS");
        }
        assert_eq!(tool_foreground_timeout(), Duration::from_secs(10));

        unsafe {
            std::env::set_var("REMI_TOOL_FOREGROUND_TIMEOUT_MS", "2500");
        }
        assert_eq!(tool_foreground_timeout(), Duration::from_millis(2500));

        unsafe {
            match previous {
                Some(value) => std::env::set_var("REMI_TOOL_FOREGROUND_TIMEOUT_MS", value),
                None => std::env::remove_var("REMI_TOOL_FOREGROUND_TIMEOUT_MS"),
            }
        }
    }

    #[test]
    fn background_task_tool_result_only_reports_existing_task() {
        let result = background_task_tool_result("task-1", "bash").preview;
        assert!(result.contains("Tool `bash` is still running in the background."));
        assert!(result.contains("task_id: task-1"));
        assert!(result.contains("Do not call the same tool again"));
        assert!(result.contains("automatically send you the completed result"));
        assert!(crate::runtime::ASYNC_TOOL_SYSTEM_PROMPT.contains(
            "You are free to process other user request, any other tasks or patiently waiting by doing nothing."
        ));
        assert!(crate::runtime::ASYNC_TOOL_SYSTEM_PROMPT.contains("automatically send"));
        assert!(!crate::runtime::ASYNC_TOOL_SYSTEM_PROMPT.contains("Background sub-agent"));
    }

    #[test]
    fn interrupted_tool_completion_fills_only_missing_results() {
        let mut state = AgentState::new(StepConfig::new("test-model"));
        state.messages = vec![
            Message::assistant_with_tool_calls(
                "",
                vec![
                    test_tool_call_message("call-1", "read"),
                    test_tool_call_message("call-2", "write"),
                    test_tool_call_message("call-3", "search"),
                ],
                None,
            ),
            Message::tool_result("call-1", "done"),
        ];
        let mut outcomes = vec![ToolCallOutcome::Error {
            tool_call_id: "call-2".to_string(),
            tool_name: "write".to_string(),
            error: "already failed".to_string(),
        }];
        let tool_calls = vec![
            test_parsed_tool_call("call-1", "read"),
            test_parsed_tool_call("call-2", "write"),
            test_parsed_tool_call("call-3", "search"),
        ];

        complete_interrupted_tool_results(&mut state, &tool_calls, &mut outcomes);
        complete_interrupted_tool_results(&mut state, &tool_calls, &mut Vec::new());

        let result_ids = state
            .messages
            .iter()
            .filter_map(|message| message.tool_call_id.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(result_ids, vec!["call-1", "call-2", "call-3"]);
        assert!(state
            .messages
            .iter()
            .any(|message| message.tool_call_id.as_deref() == Some("call-3")
                && message
                    .content
                    .text_content()
                    .contains(INTERRUPTED_TOOL_RESULT_ERROR)));
    }

    #[test]
    fn pending_tool_call_completion_inserts_result_before_next_message() {
        let mut state = AgentState::new(StepConfig::new("test-model"));
        state.messages = vec![
            Message::assistant_with_tool_calls(
                "",
                vec![test_tool_call_message("call-1", "read")],
                None,
            ),
            Message::user("next"),
        ];

        complete_pending_tool_calls_in_state(&mut state);

        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[1].role, Role::Tool);
        assert_eq!(state.messages[1].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(state.messages[2].role, Role::User);
    }

    #[test]
    fn append_tool_results_to_history_does_not_duplicate_existing_result() {
        let mut messages = vec![
            Message::assistant_with_tool_calls(
                "",
                vec![test_tool_call_message("call-1", "read")],
                None,
            ),
            Message::tool_result("call-1", "done"),
        ];

        append_tool_results_to_history(
            &mut messages,
            vec![ToolCallOutcome::Result {
                tool_call_id: "call-1".to_string(),
                tool_name: "read".to_string(),
                content: Content::text("duplicate"),
            }],
        );

        assert_eq!(
            messages
                .iter()
                .filter(|message| message.tool_call_id.as_deref() == Some("call-1"))
                .count(),
            1
        );
    }

    fn test_steer_batch(text: &str) -> CoreSteerBatch {
        CoreSteerBatch {
            ids: vec!["steer-1".to_string()],
            content: Content::text(text),
            preview: text.to_string(),
            count: 1,
            message_metadata: None,
            user_name: None,
        }
    }

    #[test]
    fn steer_start_input_closes_pending_tool_calls_before_user_message() {
        let mut state = AgentState::new(StepConfig::new("test-model"));
        state.messages = vec![Message::assistant_with_tool_calls(
            "",
            vec![test_tool_call_message("call-1", "read")],
            None,
        )];

        let input = steer_start_input(state, test_steer_batch("steer"));

        match input {
            LoopInput::Start {
                history, message, ..
            } => {
                assert_eq!(history.len(), 2);
                assert_eq!(history[1].role, Role::Tool);
                assert_eq!(history[1].tool_call_id.as_deref(), Some("call-1"));
                assert!(history[1]
                    .content
                    .text_content()
                    .contains(INTERRUPTED_TOOL_RESULT_ERROR));
                assert_eq!(message.content.text_content(), "steer");
            }
            other => panic!("expected start input, got {other:?}"),
        }
    }

    #[test]
    fn steer_after_tool_results_preserves_results_and_closes_remaining_calls() {
        let mut state = AgentState::new(StepConfig::new("test-model"));
        state.messages = vec![Message::assistant_with_tool_calls(
            "",
            vec![
                test_tool_call_message("call-1", "read"),
                test_tool_call_message("call-2", "write"),
            ],
            None,
        )];
        let results = vec![ToolCallOutcome::Result {
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            content: Content::text("done"),
        }];

        let input = steer_start_input_after_tool_results(state, results, test_steer_batch("steer"));

        match input {
            LoopInput::Start { history, .. } => {
                let result_messages = history
                    .iter()
                    .filter_map(|message| {
                        Some((
                            message.tool_call_id.as_deref()?,
                            message.content.text_content(),
                        ))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(result_messages.len(), 2);
                assert!(result_messages
                    .iter()
                    .any(|(id, text)| *id == "call-1" && text == "done"));
                assert!(result_messages.iter().any(|(id, text)| {
                    *id == "call-2" && text.contains(INTERRUPTED_TOOL_RESULT_ERROR)
                }));
            }
            other => panic!("expected start input, got {other:?}"),
        }
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
            "",
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
    async fn large_text_results_spill_to_chunk_files() {
        let root = test_root();
        let text = "x".repeat(64);
        let collected = collect_result_with_overflow(
            Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                text.clone(),
            )]))),
            &root,
            8,
            None,
            None,
            "",
        )
        .await;

        let preview = collected.preview;
        assert!(preview.contains("[Output too large (64 bytes)"));
        assert!(preview.contains("split into 64 chunk files"));
        assert!(preview.contains("there is no complete single output file"));
        assert!(matches!(collected.content, Content::Text(ref text) if text == &preview));

        let mut entries = tokio::fs::read_dir(root.join("tmp"))
            .await
            .expect("tmp dir should exist after overflow spill");
        let spill_dir = entries
            .next_entry()
            .await
            .expect("tmp dir should be readable")
            .expect("chunk spill dir should exist")
            .path();
        assert!(spill_dir.is_dir());
        assert!(entries
            .next_entry()
            .await
            .expect("tmp dir should be readable")
            .is_none());

        let mut part_entries = tokio::fs::read_dir(&spill_dir)
            .await
            .expect("spill dir should be readable");
        let mut parts = Vec::new();
        while let Some(entry) = part_entries
            .next_entry()
            .await
            .expect("spill dir entry should be readable")
        {
            parts.push(entry.path());
        }
        parts.sort();
        assert_eq!(parts.len(), 64);
        assert_eq!(
            parts[0].file_name().and_then(|name| name.to_str()),
            Some("part_0001_of_0064.txt")
        );
        assert_eq!(
            parts[63].file_name().and_then(|name| name.to_str()),
            Some("part_0064_of_0064.txt")
        );
        assert!(preview.contains(&format!("path=\"{}\"", parts[0].display())));
        assert!(preview.contains(&format!("path=\"{}\"", parts[63].display())));

        let mut reconstructed = String::new();
        for part in parts {
            let chunk = tokio::fs::read_to_string(&part)
                .await
                .expect("chunk should be readable");
            assert!(chunk.len() <= tool_output_chunk_bytes(8));
            reconstructed.push_str(&chunk);
        }
        assert_eq!(reconstructed, text);
        assert!(tokio::fs::metadata(spill_dir.with_extension("txt"))
            .await
            .is_err());
    }

    #[test]
    fn split_utf8_chunks_preserves_character_boundaries() {
        let text = "a你b🙂c";
        let chunks = split_utf8_chunks(text, 4);

        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4));
        assert_eq!(chunks, vec!["a你", "b", "🙂", "c"]);
    }

    #[tokio::test]
    async fn fs_read_overflow_returns_retry_hint_without_spilling_file() {
        let root = test_root();
        let collected = collect_result_with_overflow(
            Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                "x".repeat(64),
            )]))),
            &root,
            8,
            None,
            None,
            "fs_read",
        )
        .await;

        let preview = collected.preview;
        assert!(preview.contains("[fs_read output too large (64 bytes)"));
        assert!(preview.contains("not saved to a temporary file"));
        assert!(preview.contains("Retry fs_read"));
        assert_eq!(preview, fs_read_overflow_summary(64, 8));
        assert!(tokio::fs::metadata(root.join("tmp")).await.is_err());
    }

    fn delayed_boxed_result(
        label: &'static str,
        delay: Duration,
    ) -> Result<BoxedToolResult, remi_agentloop::prelude::AgentError> {
        let output: BoxedToolStream = Box::pin(async_stream::stream! {
            tokio::time::sleep(delay).await;
            yield ToolOutput::text(label);
        });
        Ok(ToolResult::Output(output))
    }

    #[tokio::test]
    async fn tool_result_streams_are_collected_in_parallel() {
        let root = test_root();
        let started = Instant::now();
        let tool_names = HashMap::from([
            ("call_a".to_string(), "lazy_wait".to_string()),
            ("call_b".to_string(), "lazy_wait".to_string()),
        ]);
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
            &tool_names,
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
        let output: BoxedToolStream = Box::pin(async_stream::stream! {
            yield ToolOutput::SubSession(remi_agentloop::prelude::SubSessionEvent::new(
                "",
                remi_agentloop::prelude::ThreadId("sub-thread".to_string()),
                remi_agentloop::prelude::RunId("sub-run".to_string()),
                "coder",
                None,
                1,
                remi_agentloop::prelude::ProtocolEvent::ToolCallStart {
                    id: "sub-call".to_string(),
                    name: "fs_ls".to_string(),
                },
            ));
            tokio::time::sleep(Duration::from_millis(200)).await;
            yield ToolOutput::text("done");
        });
        let tool_names = HashMap::from([("call".to_string(), "sub_agent".to_string())]);
        let mut collect_fut = std::pin::pin!(collect_tool_results_parallel(
            vec![("call".to_string(), Ok(ToolResult::Output(output)))],
            &tool_names,
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

    #[tokio::test]
    async fn tool_output_delta_streams_as_text_side_event() {
        let root = test_root();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let output = futures::stream::iter(vec![
            ToolOutput::Delta("part 1".to_string()),
            ToolOutput::Delta("part 2".to_string()),
            ToolOutput::text("part 1part 2"),
        ]);
        let collected = collect_result_with_overflow(
            Ok(ToolResult::Output(output)),
            &root,
            8_192,
            Some(tx),
            Some("call"),
            "agent__worker",
        )
        .await;

        assert_eq!(collected.preview, "part 1part 2");
        assert!(collected.side_events.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Ok(CatEvent::Text(text)) if text == "part 1"
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(CatEvent::Text(text)) if text == "part 2"
        ));
    }

    struct ParallelToolInnerAgent;

    impl Agent for ParallelToolInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _ctx: bot_runtime_core::ChatCtx,
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
            }
        }
    }

    struct MissingToolInnerAgent;

    impl Agent for MissingToolInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _ctx: bot_runtime_core::ChatCtx,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start { .. } => Ok(stream::iter(vec![
                    remi_agentloop::types::AgentEvent::NeedToolExecution {
                        state: AgentState::new(StepConfig::new("test-model")),
                        tool_calls: vec![ParsedToolCall {
                            id: "read-call".to_string(),
                            name: "read".to_string(),
                            arguments: serde_json::json!({
                                "path": "/home/skye/remi-cat/.remi-cat/agents/explorer.md"
                            }),
                        }],
                        completed_results: vec![],
                    },
                ])),
                LoopInput::Resume { results, .. } => {
                    let error = results
                        .iter()
                        .find_map(|result| match result {
                            ToolCallOutcome::Error { error, .. } => Some(error.clone()),
                            ToolCallOutcome::Result { .. } => None,
                        })
                        .unwrap_or_default();
                    Ok(stream::iter(vec![
                        remi_agentloop::types::AgentEvent::TextDelta(error),
                        remi_agentloop::types::AgentEvent::Done,
                    ]))
                }
            }
        }
    }

    struct DisallowedToolInnerAgent;

    impl Agent for DisallowedToolInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _ctx: bot_runtime_core::ChatCtx,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start { .. } => Ok(stream::iter(vec![
                    remi_agentloop::types::AgentEvent::NeedToolExecution {
                        state: AgentState::new(StepConfig::new("test-model")),
                        tool_calls: vec![ParsedToolCall {
                            id: "instant-call".to_string(),
                            name: "instant".to_string(),
                            arguments: serde_json::json!({}),
                        }],
                        completed_results: vec![],
                    },
                ])),
                LoopInput::Resume { results, .. } => {
                    let error = results
                        .iter()
                        .find_map(|result| match result {
                            ToolCallOutcome::Error { error, .. } => Some(error.clone()),
                            ToolCallOutcome::Result { .. } => None,
                        })
                        .unwrap_or_default();
                    Ok(stream::iter(vec![
                        remi_agentloop::types::AgentEvent::TextDelta(error),
                        remi_agentloop::types::AgentEvent::Done,
                    ]))
                }
            }
        }
    }

    struct ApprovalDeniedInnerAgent;

    impl Agent for ApprovalDeniedInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _ctx: bot_runtime_core::ChatCtx,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start { metadata, .. } => {
                    let mut state = AgentState::new(StepConfig::new("test-model"));
                    state.config.metadata = metadata;
                    Ok(stream::iter(vec![
                        remi_agentloop::types::AgentEvent::NeedToolExecution {
                            state,
                            tool_calls: vec![ParsedToolCall {
                                id: "send-call".to_string(),
                                name: "danger_send".to_string(),
                                arguments: serde_json::json!({ "message": "hello" }),
                            }],
                            completed_results: vec![],
                        },
                    ]))
                }
                LoopInput::Resume { .. } => Ok(stream::iter(vec![
                    remi_agentloop::types::AgentEvent::TextDelta(
                        "resumed after denial".to_string(),
                    ),
                    remi_agentloop::types::AgentEvent::Done,
                ])),
            }
        }
    }

    struct RiskySendTool;

    impl Tool for RiskySendTool {
        fn name(&self) -> &str {
            "danger_send"
        }

        fn description(&self) -> &str {
            "A high-risk test send tool."
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                }
            })
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _resume: Option<remi_agentloop::types::ResumePayload>,
            _ctx: ToolContext,
        ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
            Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                "should not execute",
            )])))
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
                    "label": { "type": "string" },
                    "delay_ms": { "type": "integer" }
                },
                "required": ["label"]
            })
        }

        async fn execute(
            &self,
            arguments: serde_json::Value,
            _resume: Option<remi_agentloop::types::ResumePayload>,
            _ctx: ToolContext,
        ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
            let label = arguments
                .get("label")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let delay_ms = arguments
                .get("delay_ms")
                .and_then(|value| value.as_u64())
                .unwrap_or(300);
            Ok(ToolResult::Output(async_stream::stream! {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                yield ToolOutput::text(label);
            }))
        }
    }

    struct StaggeredParallelToolInnerAgent;

    impl Agent for StaggeredParallelToolInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _ctx: bot_runtime_core::ChatCtx,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start { .. } => Ok(stream::iter(vec![
                    remi_agentloop::types::AgentEvent::NeedToolExecution {
                        state: AgentState::new(StepConfig::new("test-model")),
                        tool_calls: vec![
                            ParsedToolCall {
                                id: "fast-call".to_string(),
                                name: "lazy_wait".to_string(),
                                arguments: serde_json::json!({ "label": "fast", "delay_ms": 50 }),
                            },
                            ParsedToolCall {
                                id: "slow-call".to_string(),
                                name: "lazy_wait".to_string(),
                                arguments: serde_json::json!({ "label": "slow", "delay_ms": 300 }),
                            },
                        ],
                        completed_results: vec![],
                    },
                ])),
                LoopInput::Resume { .. } => {
                    Ok(stream::iter(vec![remi_agentloop::types::AgentEvent::Done]))
                }
            }
        }
    }

    struct EndlessToolInnerAgent;

    impl Agent for EndlessToolInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _ctx: bot_runtime_core::ChatCtx,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            let state = match req {
                LoopInput::Start { metadata, .. } => {
                    let mut state = AgentState::new(StepConfig::new("test-model"));
                    state.config.metadata = metadata;
                    state
                }
                LoopInput::Resume { state, .. } => state,
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
            _ctx: ToolContext,
        ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
            Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                "ok",
            )])))
        }
    }

    struct SteerDuringToolInnerAgent;

    impl Agent for SteerDuringToolInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _ctx: bot_runtime_core::ChatCtx,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start {
                    message,
                    history,
                    extra_tools,
                    ..
                } if message.content.text_content().contains("while-tool steer") => {
                    assert!(
                        history.iter().any(|message| {
                            message.tool_call_id.as_deref() == Some("steer-call")
                                && message.content.text_content() == "tool done"
                        }),
                        "tool result should be materialized into history before steer turn"
                    );
                    assert!(
                        extra_tools
                            .iter()
                            .any(|definition| definition.function.name == "instant"),
                        "steer turn should retain injected tool definitions"
                    );
                    Ok(stream::iter(vec![
                        remi_agentloop::types::AgentEvent::TextDelta("steered".to_string()),
                        remi_agentloop::types::AgentEvent::Done,
                    ]))
                }
                LoopInput::Start { .. } => {
                    let mut state = AgentState::new(StepConfig::new("test-model"));
                    state.messages = vec![Message::assistant_with_tool_calls(
                        "",
                        vec![ToolCallMessage {
                            id: "steer-call".to_string(),
                            call_type: "function".to_string(),
                            function: FunctionCall {
                                name: "instant".to_string(),
                                arguments: "{}".to_string(),
                            },
                        }],
                        None,
                    )];
                    Ok(stream::iter(vec![
                        remi_agentloop::types::AgentEvent::NeedToolExecution {
                            state,
                            tool_calls: vec![ParsedToolCall {
                                id: "steer-call".to_string(),
                                name: "instant".to_string(),
                                arguments: serde_json::json!({}),
                            }],
                            completed_results: vec![],
                        },
                    ]))
                }
                LoopInput::Resume { .. } => {
                    panic!("tool-gap steer should start a steer turn instead of resuming")
                }
            }
        }
    }

    struct SteerPushingTool {
        queue: Arc<CoreSteerQueue>,
    }

    impl Tool for SteerPushingTool {
        fn name(&self) -> &str {
            "instant"
        }

        fn description(&self) -> &str {
            "Queues steer while the tool is running."
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _resume: Option<remi_agentloop::types::ResumePayload>,
            _ctx: ToolContext,
        ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
            self.queue.push(CoreSteerInput {
                id: "queued-while-tool".to_string(),
                content: Content::text("while-tool steer"),
                preview: "while-tool steer".to_string(),
                message_metadata: None,
                user_name: None,
                source: CoreSteerSource::User,
            });
            Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
                "tool done",
            )])))
        }
    }

    struct UsageThenDoneInnerAgent;

    impl Agent for UsageThenDoneInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _ctx: bot_runtime_core::ChatCtx,
            _req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            Ok(stream::iter(vec![
                remi_agentloop::types::AgentEvent::Usage {
                    prompt_tokens: 12,
                    completion_tokens: 3,
                },
                remi_agentloop::types::AgentEvent::TextDelta("ok".to_string()),
                remi_agentloop::types::AgentEvent::Done,
            ]))
        }
    }

    struct CancelledWithCheckpointInnerAgent;

    impl Agent for CancelledWithCheckpointInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            _ctx: bot_runtime_core::ChatCtx,
            _req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            let mut state = AgentState::new(StepConfig::new("test-model"));
            state.messages = vec![Message::user("hello"), Message::assistant("partial")];
            Ok(stream::iter(vec![
                remi_agentloop::types::AgentEvent::Checkpoint(Checkpoint::new(
                    state.thread_id.clone(),
                    state.run_id.clone(),
                    state,
                    None,
                    1,
                    CheckpointStatus::Cancelled,
                    1,
                )),
                remi_agentloop::types::AgentEvent::Cancelled,
            ]))
        }
    }

    #[tokio::test]
    async fn usage_events_emit_stats_before_done() {
        let agent = CatAgent {
            inner: UsageThenDoneInnerAgent,
            local_tools: Arc::new(DefaultToolRegistry::new()),
            model_tools: None,
            data_dir: test_root(),
            workspace_root: test_root(),
            workspace_root_label: "/workspace".to_string(),
            allow_host_absolute_paths: true,
            overflow_bytes: 8_192,
            im_bridge: None,
            tool_allowlist: None,
            approval_manager: ToolApprovalManager::new(),
            approval_reviewer: None,
            user_question_manager: UserQuestionManager::new(),
            hook_manager: test_hook_manager(),
            tool_tasks: test_tool_tasks(),
        };

        let events = agent
            .stream_with_input(LoopInput::start("usage"))
            .collect::<Vec<_>>()
            .await;
        let stats_index = events
            .iter()
            .position(|event| matches!(event, CatEvent::Stats { .. }))
            .expect("stats event should be emitted");
        let done_index = events
            .iter()
            .position(|event| matches!(event, CatEvent::Done))
            .expect("done event should be emitted");

        assert!(stats_index < done_index);
        assert!(matches!(
            &events[stats_index],
            CatEvent::Stats {
                prompt_tokens: 12,
                completion_tokens: 3,
                max_prompt_tokens: 12,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn cancelled_agent_event_emits_history_before_done() {
        let agent = CatAgent {
            inner: CancelledWithCheckpointInnerAgent,
            local_tools: Arc::new(DefaultToolRegistry::new()),
            model_tools: None,
            data_dir: test_root(),
            workspace_root: test_root(),
            workspace_root_label: "/workspace".to_string(),
            allow_host_absolute_paths: true,
            overflow_bytes: 8_192,
            im_bridge: None,
            tool_allowlist: None,
            approval_manager: ToolApprovalManager::new(),
            approval_reviewer: None,
            user_question_manager: UserQuestionManager::new(),
            hook_manager: test_hook_manager(),
            tool_tasks: test_tool_tasks(),
        };

        let events = agent
            .stream_with_input(LoopInput::start("cancel"))
            .collect::<Vec<_>>()
            .await;
        let history_index = events
            .iter()
            .position(|event| matches!(event, CatEvent::History(_, _)))
            .expect("history event should be emitted");
        let done_index = events
            .iter()
            .position(|event| matches!(event, CatEvent::Done))
            .expect("done event should be emitted");

        assert!(history_index < done_index);
        assert!(matches!(
            &events[history_index],
            CatEvent::History(messages, _) if messages
                .last()
                .is_some_and(|message| message.content.text_content() == "partial")
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cat_agent_collects_parallel_tool_streams_end_to_end() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut local_tools = DefaultToolRegistry::new();
                local_tools.register(LazyWaitTool);
                let agent = CatAgent {
                    inner: ParallelToolInnerAgent,
                    local_tools: Arc::new(local_tools),
                    model_tools: None,
                    data_dir: test_root(),
                    workspace_root: test_root(),
                    workspace_root_label: "/workspace".to_string(),
                    allow_host_absolute_paths: true,
                    overflow_bytes: 8_192,
                    im_bridge: None,
                    tool_allowlist: None,
                    approval_manager: ToolApprovalManager::new(),
                    approval_reviewer: None,
                    user_question_manager: UserQuestionManager::new(),
                    hook_manager: test_hook_manager(),
                    tool_tasks: test_tool_tasks(),
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
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cat_agent_streams_parallel_tool_results_as_each_finishes() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut local_tools = DefaultToolRegistry::new();
                local_tools.register(LazyWaitTool);
                let agent = CatAgent {
                    inner: StaggeredParallelToolInnerAgent,
                    local_tools: Arc::new(local_tools),
                    model_tools: None,
                    data_dir: test_root(),
                    workspace_root: test_root(),
                    workspace_root_label: "/workspace".to_string(),
                    allow_host_absolute_paths: true,
                    overflow_bytes: 8_192,
                    im_bridge: None,
                    tool_allowlist: None,
                    approval_manager: ToolApprovalManager::new(),
                    approval_reviewer: None,
                    user_question_manager: UserQuestionManager::new(),
                    hook_manager: test_hook_manager(),
                    tool_tasks: test_tool_tasks(),
                };

                let started = Instant::now();
                let stream = agent.stream_with_input(LoopInput::start("run staggered tools"));
                tokio::pin!(stream);
                let mut saw_slow_before_fast = false;

                while let Some(event) = stream.next().await {
                    match event {
                        CatEvent::ToolCallResult { id, .. } if id == "slow-call" => {
                            saw_slow_before_fast = true;
                        }
                        CatEvent::ToolCallResult { id, result, .. } if id == "fast-call" => {
                            assert_eq!(result, "fast");
                            assert!(
                                started.elapsed() < Duration::from_millis(200),
                                "fast tool result was delayed until the slow tool finished"
                            );
                            assert!(!saw_slow_before_fast);
                            return;
                        }
                        _ => {}
                    }
                }

                panic!("fast tool result was not emitted");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timed_out_tool_result_runs_as_background_task_and_notifies() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let root = test_root();
                let tool_tasks = test_tool_tasks();
                let mut completed_rx = tool_tasks.subscribe_completed();
                let tool_names =
                    HashMap::from([("slow-call".to_string(), "lazy_wait".to_string())]);
                let calls = vec![ParsedToolCall {
                    id: "slow-call".to_string(),
                    name: "lazy_wait".to_string(),
                    arguments: serde_json::json!({"label": "slow"}),
                }];

                let mut pending = collect_tool_result_futures_with_timeout(
                    vec![(
                        "slow-call".to_string(),
                        delayed_boxed_result("slow", Duration::from_millis(50)),
                    )],
                    &calls,
                    &tool_names,
                    &root,
                    8_192,
                    None,
                    Arc::clone(&tool_tasks),
                    "thread-1".to_string(),
                    "run-1".to_string(),
                    Duration::from_millis(5),
                    true,
                );

                let outcome = pending
                    .next()
                    .await
                    .expect("tool result future should produce an outcome");
                let ForegroundToolOutcome::TimedOut { call_id, task_id } = outcome else {
                    panic!("expected slow tool to exceed the foreground window");
                };
                assert_eq!(call_id, "slow-call");
                assert!(tool_tasks.is_thread_running("thread-1").await);

                let completed = tokio::time::timeout(Duration::from_secs(1), completed_rx.recv())
                    .await
                    .expect("background completion should notify")
                    .expect("completion channel should stay open");
                assert_eq!(completed.task_id, task_id);
                assert_eq!(completed.tool_name, "lazy_wait");
                assert_eq!(completed.status.as_str(), "completed");
                assert_eq!(completed.recent_output, vec!["slow".to_string()]);
                assert!(!tool_tasks.is_thread_running("thread-1").await);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manual_background_task_does_not_auto_notify() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let root = test_root();
                let tool_tasks = test_tool_tasks();
                let mut completed_rx = tool_tasks.subscribe_completed();
                let tool_names =
                    HashMap::from([("slow-call".to_string(), "lazy_wait".to_string())]);
                let calls = vec![ParsedToolCall {
                    id: "slow-call".to_string(),
                    name: "lazy_wait".to_string(),
                    arguments: serde_json::json!({"label": "slow"}),
                }];

                let mut pending = collect_tool_result_futures_with_timeout(
                    vec![(
                        "slow-call".to_string(),
                        delayed_boxed_result("slow", Duration::from_millis(30)),
                    )],
                    &calls,
                    &tool_names,
                    &root,
                    8_192,
                    None,
                    Arc::clone(&tool_tasks),
                    "thread-1".to_string(),
                    "run-1".to_string(),
                    Duration::from_millis(5),
                    false,
                );

                let outcome = pending
                    .next()
                    .await
                    .expect("tool result future should produce an outcome");
                let ForegroundToolOutcome::TimedOut { task_id, .. } = outcome else {
                    panic!("expected slow tool to exceed the foreground window");
                };
                assert!(tool_tasks.get(&task_id).await.is_some());

                tokio::time::sleep(Duration::from_millis(60)).await;
                assert!(!tool_tasks.is_thread_running("thread-1").await);
                assert!(
                    tokio::time::timeout(Duration::from_millis(25), completed_rx.recv())
                        .await
                        .is_err()
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn foreground_completed_tool_updates_task_without_completion_notification() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let root = test_root();
                let tool_tasks = test_tool_tasks();
                let mut completed_rx = tool_tasks.subscribe_completed();
                let tool_names =
                    HashMap::from([("fast-call".to_string(), "lazy_wait".to_string())]);
                let calls = vec![ParsedToolCall {
                    id: "fast-call".to_string(),
                    name: "lazy_wait".to_string(),
                    arguments: serde_json::json!({"label": "fast"}),
                }];

                let mut pending = collect_tool_result_futures_with_timeout(
                    vec![(
                        "fast-call".to_string(),
                        delayed_boxed_result("fast", Duration::from_millis(5)),
                    )],
                    &calls,
                    &tool_names,
                    &root,
                    8_192,
                    None,
                    Arc::clone(&tool_tasks),
                    "thread-1".to_string(),
                    "run-1".to_string(),
                    Duration::from_millis(200),
                    true,
                );

                let outcome = pending
                    .next()
                    .await
                    .expect("tool result future should produce an outcome");
                let ForegroundToolOutcome::Completed { call_id, collected } = outcome else {
                    panic!("expected fast tool to complete inside the foreground window");
                };
                assert_eq!(call_id, "fast-call");
                assert_eq!(collected.preview, "fast");
                assert!(!tool_tasks.is_thread_running("thread-1").await);

                let task = tool_tasks
                    .list(Some("thread-1"))
                    .await
                    .into_iter()
                    .find(|task| task.tool_call_id == "fast-call")
                    .expect("foreground-completed task should still be recorded");
                assert_eq!(task.status.as_str(), "completed");
                assert_eq!(task.result_preview.as_deref(), Some("fast"));
                assert!(
                    tokio::time::timeout(Duration::from_millis(25), completed_rx.recv())
                        .await
                        .is_err()
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timed_out_tool_result_forwards_background_side_events() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let root = test_root();
                let tool_tasks = test_tool_tasks();
                let mut side_events = tool_tasks.subscribe_side_events();
                let (side_tx, side_rx) = mpsc::unbounded_channel();
                let tool_names = HashMap::from([("sub-call".to_string(), "sub_agent".to_string())]);
                let calls = vec![ParsedToolCall {
                    id: "sub-call".to_string(),
                    name: "sub_agent".to_string(),
                    arguments: serde_json::json!({"task": "run bash"}),
                }];
                let output: BoxedToolStream = Box::pin(async_stream::stream! {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    yield ToolOutput::SubSession(remi_agentloop::prelude::SubSessionEvent::new(
                        "",
                        remi_agentloop::prelude::ThreadId("sub-thread".to_string()),
                        remi_agentloop::prelude::RunId("sub-run".to_string()),
                        "coder",
                        None,
                        1,
                        remi_agentloop::prelude::ProtocolEvent::ToolCallStart {
                            id: "bash-call".to_string(),
                            name: "bash".to_string(),
                        },
                    ));
                    yield ToolOutput::text("done");
                });

                let mut pending = collect_tool_result_futures_with_timeout(
                    vec![("sub-call".to_string(), Ok(ToolResult::Output(output)))],
                    &calls,
                    &tool_names,
                    &root,
                    8_192,
                    Some(side_tx),
                    Arc::clone(&tool_tasks),
                    "thread-1".to_string(),
                    "run-1".to_string(),
                    Duration::from_millis(5),
                    true,
                );

                let outcome = pending
                    .next()
                    .await
                    .expect("tool result future should produce an outcome");
                let ForegroundToolOutcome::TimedOut { task_id: _, .. } = outcome else {
                    panic!("expected sub-agent tool to exceed the foreground window");
                };

                spawn_background_side_event_forwarder(
                    "thread-1".to_string(),
                    Arc::clone(&tool_tasks),
                    side_rx,
                );

                let (thread_id, event) =
                    tokio::time::timeout(Duration::from_secs(1), side_events.recv())
                        .await
                        .expect("background side event should notify")
                        .expect("side event channel should stay open");
                assert_eq!(thread_id, "thread-1");
                assert!(matches!(
                    event,
                    CatEvent::SubSession(ref sub)
                        if matches!(
                            sub.event.as_ref(),
                            remi_agentloop::prelude::ProtocolEvent::ToolCallStart { name, .. }
                                if name == "bash"
                        )
                ));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timed_out_subagent_approval_request_forwards_as_background_side_event() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let root = test_root();
                let tool_tasks = test_tool_tasks();
                let mut side_events = tool_tasks.subscribe_side_events();
                let (side_tx, side_rx) = mpsc::unbounded_channel();
                let tool_names = HashMap::from([("sub-call".to_string(), "sub_agent".to_string())]);
                let calls = vec![ParsedToolCall {
                    id: "sub-call".to_string(),
                    name: "sub_agent".to_string(),
                    arguments: serde_json::json!({"task": "run bash"}),
                }];
                let output: BoxedToolStream = Box::pin(async_stream::stream! {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    yield crate::tool_approval_requested_marker(
                        &remi_agentloop::prelude::ThreadId("sub-thread".to_string()),
                        &remi_agentloop::prelude::RunId("sub-run".to_string()),
                        &crate::ToolApprovalRequest {
                            id: "approval-1".to_string(),
                            session_id: "thread-1".to_string(),
                            run_id: "run-1".to_string(),
                            tool_call_id: "bash-call".to_string(),
                            tool_name: "bash".to_string(),
                            risk: crate::ToolRiskLevel::High,
                            args_summary: "sleep 60".to_string(),
                            command_key: Some("bash:sleep 60".to_string()),
                            model_review_reason: None,
                            platform: Some("tui".to_string()),
                            review: None,
                        },
                    );
                    yield ToolOutput::text("done");
                });

                let mut pending = collect_tool_result_futures_with_timeout(
                    vec![("sub-call".to_string(), Ok(ToolResult::Output(output)))],
                    &calls,
                    &tool_names,
                    &root,
                    8_192,
                    Some(side_tx),
                    Arc::clone(&tool_tasks),
                    "thread-1".to_string(),
                    "run-1".to_string(),
                    Duration::from_millis(5),
                    true,
                );

                let outcome = pending
                    .next()
                    .await
                    .expect("tool result future should produce an outcome");
                let ForegroundToolOutcome::TimedOut { .. } = outcome else {
                    panic!("expected sub-agent tool to exceed the foreground window");
                };

                spawn_background_side_event_forwarder(
                    "thread-1".to_string(),
                    Arc::clone(&tool_tasks),
                    side_rx,
                );

                let (thread_id, event) =
                    tokio::time::timeout(Duration::from_secs(1), side_events.recv())
                        .await
                        .expect("background approval event should notify")
                        .expect("side event channel should stay open");
                assert_eq!(thread_id, "thread-1");
                assert!(matches!(
                    event,
                    CatEvent::ToolApprovalRequested(ref request)
                        if request.tool_call_id == "bash-call"
                            && request.tool_name == "bash"
                            && request.platform.as_deref() == Some("tui")
                ));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cat_agent_cancel_during_tool_collection_emits_repaired_history() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut local_tools = DefaultToolRegistry::new();
                local_tools.register(LazyWaitTool);
                let agent = CatAgent {
                    inner: StaggeredParallelToolInnerAgent,
                    local_tools: Arc::new(local_tools),
                    model_tools: None,
                    data_dir: test_root(),
                    workspace_root: test_root(),
                    workspace_root_label: "/workspace".to_string(),
                    allow_host_absolute_paths: true,
                    overflow_bytes: 8_192,
                    im_bridge: None,
                    tool_allowlist: None,
                    approval_manager: ToolApprovalManager::new(),
                    approval_reviewer: None,
                    user_question_manager: UserQuestionManager::new(),
                    hook_manager: test_hook_manager(),
                    tool_tasks: test_tool_tasks(),
                };
                let cancel = CancellationToken::new();
                let stream = agent.stream_with_input_and_options(
                    LoopInput::start("run staggered tools"),
                    CoreStreamOptions {
                        cancel: Some(cancel.clone()),
                        steer: None,
                        async_agent: false,
                    },
                );
                tokio::pin!(stream);

                let mut repaired_history = None;
                let mut saw_cancelled = false;
                while let Some(event) = stream.next().await {
                    match event {
                        CatEvent::ToolCallResult { id, .. } if id == "fast-call" => {
                            cancel.cancel();
                        }
                        CatEvent::History(messages, _) => {
                            repaired_history = Some(messages);
                        }
                        CatEvent::Cancelled => {
                            saw_cancelled = true;
                            break;
                        }
                        CatEvent::Error(err) => panic!("unexpected error: {err}"),
                        _ => {}
                    }
                }

                assert!(saw_cancelled);
                let history = repaired_history.expect("cancel should emit repaired history");
                let result_messages = history
                    .iter()
                    .filter_map(|message| {
                        message
                            .tool_call_id
                            .as_deref()
                            .map(|id| (id.to_string(), message.content.text_content()))
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    result_messages
                        .iter()
                        .filter(|(id, _)| id == "fast-call")
                        .count(),
                    1
                );
                assert_eq!(
                    result_messages
                        .iter()
                        .filter(|(id, _)| id == "slow-call")
                        .count(),
                    1
                );
                assert!(result_messages
                    .iter()
                    .any(|(id, text)| id == "fast-call" && text == "fast"));
                assert!(result_messages.iter().any(|(id, text)| {
                    id == "slow-call" && text.contains(INTERRUPTED_TOOL_RESULT_ERROR)
                }));
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cat_agent_injects_steer_after_tool_results_at_tool_gap() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let queue = Arc::new(CoreSteerQueue::new());
                let mut local_tools = DefaultToolRegistry::new();
                local_tools.register(SteerPushingTool {
                    queue: Arc::clone(&queue),
                });
                let agent = CatAgent {
                    inner: SteerDuringToolInnerAgent,
                    local_tools: Arc::new(local_tools),
                    model_tools: None,
                    data_dir: test_root(),
                    workspace_root: test_root(),
                    workspace_root_label: "/workspace".to_string(),
                    allow_host_absolute_paths: true,
                    overflow_bytes: 8_192,
                    im_bridge: None,
                    tool_allowlist: None,
                    approval_manager: ToolApprovalManager::new(),
                    approval_reviewer: None,
                    user_question_manager: UserQuestionManager::new(),
                    hook_manager: test_hook_manager(),
                    tool_tasks: test_tool_tasks(),
                };

                let events = agent
                    .stream_with_input_and_options(
                        LoopInput::start("start"),
                        CoreStreamOptions::new().with_steer(queue),
                    )
                    .collect::<Vec<_>>()
                    .await;

                assert!(
                    events.iter().any(
                        |event| matches!(event, CatEvent::SteerInjected(event) if event.count == 1)
                    ),
                    "{events:#?}"
                );
                assert!(events
                    .iter()
                    .any(|event| matches!(event, CatEvent::Text(text) if text == "steered")));
            })
            .await;
    }

    #[tokio::test]
    async fn missing_tool_emits_precise_failed_tool_result_and_resumes() {
        let agent = CatAgent {
            inner: MissingToolInnerAgent,
            local_tools: Arc::new(DefaultToolRegistry::new()),
            model_tools: None,
            data_dir: test_root(),
            workspace_root: test_root(),
            workspace_root_label: "/workspace".to_string(),
            allow_host_absolute_paths: true,
            overflow_bytes: 8_192,
            im_bridge: None,
            tool_allowlist: None,
            approval_manager: ToolApprovalManager::new(),
            approval_reviewer: None,
            user_question_manager: UserQuestionManager::new(),
            hook_manager: test_hook_manager(),
            tool_tasks: test_tool_tasks(),
        };

        let events = agent
            .stream_with_input(LoopInput::start("call read"))
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| {
            matches!(
                event,
                CatEvent::ToolCall { id, name, .. } if id == "read-call" && name == "read"
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                CatEvent::ToolCallResult { id, name, success, result, .. }
                    if id == "read-call"
                        && name == "read"
                        && !success
                        && result.contains("tool not found: read")
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                CatEvent::Text(text) if text.contains("tool not found: read")
            )
        }));
        assert!(events.iter().any(|event| matches!(event, CatEvent::Done)));
        assert!(!events
            .iter()
            .any(|event| matches!(event, CatEvent::Error(_))));
    }

    #[tokio::test]
    async fn disallowed_tool_emits_precise_failed_tool_result_and_resumes() {
        let mut local_tools = DefaultToolRegistry::new();
        local_tools.register(InstantTool);
        let agent = CatAgent {
            inner: DisallowedToolInnerAgent,
            local_tools: Arc::new(local_tools),
            model_tools: None,
            data_dir: test_root(),
            workspace_root: test_root(),
            workspace_root_label: "/workspace".to_string(),
            allow_host_absolute_paths: true,
            overflow_bytes: 8_192,
            im_bridge: None,
            tool_allowlist: Some(vec!["rg".to_string()]),
            approval_manager: ToolApprovalManager::new(),
            approval_reviewer: None,
            user_question_manager: UserQuestionManager::new(),
            hook_manager: test_hook_manager(),
            tool_tasks: test_tool_tasks(),
        };

        let events = agent
            .stream_with_input(LoopInput::start("call instant"))
            .collect::<Vec<_>>()
            .await;

        assert!(events.iter().any(|event| {
            matches!(
                event,
                CatEvent::ToolCall { id, name, .. } if id == "instant-call" && name == "instant"
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                CatEvent::ToolCallResult { id, name, success, result, .. }
                    if id == "instant-call"
                        && name == "instant"
                        && !success
                        && result.contains("tool is not allowed for current agent: instant")
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                CatEvent::Text(text) if text.contains("tool is not allowed for current agent: instant")
            )
        }));
        assert!(events.iter().any(|event| matches!(event, CatEvent::Done)));
        assert!(!events
            .iter()
            .any(|event| matches!(event, CatEvent::Error(_))));
    }

    #[tokio::test]
    async fn approval_denial_aborts_without_tool_result_or_resume() {
        let mut local_tools = DefaultToolRegistry::new();
        local_tools.register(RiskySendTool);
        let agent = CatAgent {
            inner: ApprovalDeniedInnerAgent,
            local_tools: Arc::new(local_tools),
            model_tools: None,
            data_dir: test_root(),
            workspace_root: test_root(),
            workspace_root_label: "/workspace".to_string(),
            allow_host_absolute_paths: true,
            overflow_bytes: 8_192,
            im_bridge: None,
            tool_allowlist: None,
            approval_manager: ToolApprovalManager::new(),
            approval_reviewer: None,
            user_question_manager: UserQuestionManager::new(),
            hook_manager: test_hook_manager(),
            tool_tasks: test_tool_tasks(),
        };

        let events = agent
            .stream_with_input(LoopInput::start("call risky send"))
            .collect::<Vec<_>>()
            .await;

        assert!(events
            .iter()
            .any(|event| matches!(event, CatEvent::ToolApprovalRequested(request) if request.tool_call_id == "send-call")));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                CatEvent::ToolApprovalResolved { request, decision }
                    if request.tool_call_id == "send-call"
                        && *decision == crate::approval::ToolApprovalDecision::Deny
            )
        }));
        assert!(!events
            .iter()
            .any(|event| matches!(event, CatEvent::ToolCall { id, .. } if id == "send-call")));
        assert!(!events.iter().any(
            |event| matches!(event, CatEvent::ToolCallResult { id, .. } if id == "send-call")
        ));
        assert!(!events.iter().any(
            |event| matches!(event, CatEvent::Text(text) if text.contains("resumed after denial"))
        ));
        assert!(events.iter().any(|event| matches!(event, CatEvent::Done)));
        assert!(!events
            .iter()
            .any(|event| matches!(event, CatEvent::Error(_))));
    }

    #[tokio::test]
    async fn cancel_while_waiting_for_tool_approval_resolves_and_cancels() {
        let mut local_tools = DefaultToolRegistry::new();
        local_tools.register(RiskySendTool);
        let agent = CatAgent {
            inner: ApprovalDeniedInnerAgent,
            local_tools: Arc::new(local_tools),
            model_tools: None,
            data_dir: test_root(),
            workspace_root: test_root(),
            workspace_root_label: "/workspace".to_string(),
            allow_host_absolute_paths: true,
            overflow_bytes: 8_192,
            im_bridge: None,
            tool_allowlist: None,
            approval_manager: ToolApprovalManager::new(),
            approval_reviewer: None,
            user_question_manager: UserQuestionManager::new(),
            hook_manager: test_hook_manager(),
            tool_tasks: test_tool_tasks(),
        };
        let cancel = CancellationToken::new();
        let input = LoopInput::start("call risky send").metadata(serde_json::json!({
            "thread_id": "approval-cancel-thread",
            "platform": "tui",
        }));
        let stream = agent.stream_with_input_and_options(
            input,
            CoreStreamOptions {
                cancel: Some(cancel.clone()),
                steer: None,
                async_agent: false,
            },
        );
        tokio::pin!(stream);

        let mut saw_requested = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .expect("approval request should arrive")
                .expect("stream should emit approval request");
            if matches!(
                event,
                CatEvent::ToolApprovalRequested(ref request)
                    if request.tool_call_id == "send-call"
                        && request.platform.as_deref() == Some("tui")
            ) {
                saw_requested = true;
                break;
            }
        }
        assert!(saw_requested);

        cancel.cancel();
        let mut saw_resolved = false;
        let mut saw_cancelled = false;
        for _ in 0..5 {
            let Some(event) = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .expect("cancelled approval stream should make progress")
            else {
                break;
            };
            match event {
                CatEvent::ToolApprovalResolved { request, decision }
                    if request.tool_call_id == "send-call"
                        && decision == crate::approval::ToolApprovalDecision::Deny =>
                {
                    saw_resolved = true;
                }
                CatEvent::Cancelled => {
                    saw_cancelled = true;
                    break;
                }
                CatEvent::ToolCall { id, .. } if id == "send-call" => {
                    panic!("tool should not execute after approval wait is cancelled");
                }
                _ => {}
            }
        }

        assert!(saw_resolved);
        assert!(saw_cancelled);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn supervisor_run_stops_after_bounded_tool_rounds() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
        let mut local_tools = DefaultToolRegistry::new();
        local_tools.register(InstantTool);
        let agent = CatAgent {
            inner: EndlessToolInnerAgent,
            local_tools: Arc::new(local_tools),
            model_tools: None,
            data_dir: test_root(),
            workspace_root: test_root(),
            workspace_root_label: "/workspace".to_string(),
            allow_host_absolute_paths: true,
            overflow_bytes: 8_192,
            im_bridge: None,
            tool_allowlist: None,
            approval_manager: ToolApprovalManager::new(),
            approval_reviewer: None,
            user_question_manager: UserQuestionManager::new(),
            hook_manager: test_hook_manager(),
            tool_tasks: test_tool_tasks(),
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
            })
            .await;
    }
}
