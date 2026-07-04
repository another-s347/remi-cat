//! Core drive loop for `CatBot`.
//!
//! `run()` wraps any `AgentLoop` (or composable inner agent), registers
//! skill and todo tools locally, drives the inner agent, handles
//! `NeedToolExecution` by executing local tools and resuming, and finally
//! emits `CatEvent::History` + `CatEvent::Done` when the run finishes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_stream::stream;
use futures::{future::join_all, Stream, StreamExt};
use tracing::debug;

use bot_runtime_core::{
    build_tool_definition_ctx, inject_extra_tools, tool_ctx_from_state, CoreAgentLoop,
    CoreCancelKind, CoreDriveConfig, CoreDriveEvent, CoreSteerBatch, CoreStreamOptions,
    CoreUsageStats,
};
use remi_agentloop::prelude::{
    AgentError, AgentState, Content, ContentPart, LoopInput, Message, ParsedToolCall,
    ToolCallOutcome, ToolContext, ToolDefinition, ToolDefinitionContext, ToolOutput, ToolResult,
};
use remi_agentloop::tool::registry::{DefaultToolRegistry, ToolRegistry};
use remi_agentloop::tool::BoxedToolResult;
use remi_agentloop::types::{AgentEvent, ResumePayload};
use tokio::sync::mpsc;

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
                cancel: options.cancel,
                max_tool_rounds: supervisor_run.then_some(SUPERVISOR_MAX_TOOL_ROUNDS),
            });

            let mut current = input;
            let mut last_messages: Vec<Message> = vec![];
            let mut last_user_state: serde_json::Value = serde_json::Value::Null;

            loop {
                let run_input = inject_extra_tools(current, extra_defs.clone());
                let inner_stream = match self.inner.chat(run_input).await {
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

                while let Some(ev) = inner_stream.next().await {
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

                            let tool_ctx = tool_ctx_from_state(&state);
                            let mut all_outcomes: Vec<ToolCallOutcome> = completed_results;

                            if !local.is_empty() {
                                let mut approved_local = Vec::new();
                                for original_tc in &local {
                                    let mut tc = original_tc.clone();
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
                                        }
                                    };
                                    if approval_resolution == ApprovalResolution::Approved {
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
                                let results = execute_runtime_tools(
                                    &self.local_tools,
                                    self.model_tools.as_ref(),
                                    &approved_local,
                                    &resume_map,
                                    &tool_ctx,
                                )
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
                                    &tool_names,
                                    &data_dir,
                                    overflow_bytes,
                                    Some(side_tx),
                                ));
                                let collected_results = loop {
                                    tokio::select! {
                                        Some(side_event) = side_rx.recv() => {
                                            let denied = is_denied_approval_event(&side_event);
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
                                        collected = &mut collect_fut => {
                                            break collected;
                                        }
                                    }
                                };

                                for (call_id, mut collected) in collected_results {
                                    let tc = approved_local.iter().find(|t| t.id == call_id).unwrap();
                                    let elapsed_ms = started_at
                                        .get(&call_id)
                                        .map(|instant| instant.elapsed().as_millis() as u64)
                                        .unwrap_or(0);
                                    let hook_context = hook_context_from_state(
                                        &state,
                                        &workspace_root,
                                    );
                                    let post_hook = hook_manager
                                        .run_tool(
                                            HookEventName::PostToolUse,
                                            &hook_context,
                                            &tool_hook_context(
                                                tc,
                                                Some(serde_json::Value::String(collected.preview.clone())),
                                            ),
                                        )
                                        .await;
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
                                for original_tc in &dynamic {
                                    let mut tc = original_tc.clone();
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
                                        }
                                    };
                                    if approval_resolution == ApprovalResolution::Approved {
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
                                    &tool_names,
                                    &data_dir,
                                    overflow_bytes,
                                    Some(side_tx),
                                ));
                                let collected_results = loop {
                                    tokio::select! {
                                        Some(side_event) = side_rx.recv() => {
                                            let denied = is_denied_approval_event(&side_event);
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
                                        collected = &mut collect_fut => {
                                            break collected;
                                        }
                                    }
                                };

                                for (call_id, mut collected) in collected_results {
                                    let tc = approved_dynamic.iter().find(|t| t.id == call_id).unwrap();
                                    let elapsed_ms = started_at
                                        .get(&call_id)
                                        .map(|instant| instant.elapsed().as_millis() as u64)
                                        .unwrap_or(0);
                                    let hook_context = hook_context_from_state(
                                        &state,
                                        &workspace_root,
                                    );
                                    let post_hook = hook_manager
                                        .run_tool(
                                            HookEventName::PostToolUse,
                                            &hook_context,
                                            &tool_hook_context(
                                                tc,
                                                Some(serde_json::Value::String(collected.preview.clone())),
                                            ),
                                        )
                                        .await;
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

                                    all_outcomes.push(ToolCallOutcome::Result {
                                        tool_call_id: call_id,
                                        tool_name: tc.name.clone(),
                                        content: collected.content,
                                    });
                                }
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

fn steer_start_input(state: AgentState, batch: CoreSteerBatch) -> LoopInput {
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
    steer_start_input(state, batch)
}

fn append_tool_results_to_history(messages: &mut Vec<Message>, results: Vec<ToolCallOutcome>) {
    for result in results {
        match result {
            ToolCallOutcome::Result {
                tool_call_id,
                content,
                ..
            } => messages.push(Message::tool_result_content(tool_call_id, content)),
            ToolCallOutcome::Error {
                tool_call_id,
                error,
                ..
            } => messages.push(Message::tool_result(
                tool_call_id,
                format!("error: {error}"),
            )),
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
) -> Vec<(String, Result<BoxedToolResult<'a>, AgentError>)> {
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
        .collect::<HashMap<String, Result<BoxedToolResult<'a>, AgentError>>>();
    let mut model_by_id = model_results
        .into_iter()
        .collect::<HashMap<String, Result<BoxedToolResult<'a>, AgentError>>>();
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
        approval_request_for_tool, collect_result_with_overflow, collect_tool_results_parallel,
        fs_read_overflow_summary, split_utf8_chunks, tool_output_chunk_bytes, CatAgent,
    };
    use crate::approval::ToolApprovalManager;
    use crate::events::CatEvent;
    use crate::user_question::UserQuestionManager;
    use bot_runtime_core::{CoreSteerInput, CoreSteerQueue, CoreStreamOptions};
    use futures::{stream, Stream, StreamExt};
    use remi_agentloop::prelude::{
        Agent, AgentError, AgentState, Checkpoint, CheckpointStatus, Content, ContentPart,
        LoopInput, Message, ParsedToolCall, StepConfig, ToolCallOutcome, ToolContext, ToolOutput,
        ToolResult,
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

    struct MissingToolInnerAgent;

    impl Agent for MissingToolInnerAgent {
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
                LoopInput::Cancel { .. } => {
                    Ok(stream::iter(vec![remi_agentloop::types::AgentEvent::Done]))
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
                LoopInput::Cancel { .. } => {
                    Ok(stream::iter(vec![remi_agentloop::types::AgentEvent::Done]))
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
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start { .. } => Ok(stream::iter(vec![
                    remi_agentloop::types::AgentEvent::NeedToolExecution {
                        state: AgentState::new(StepConfig::new("test-model")),
                        tool_calls: vec![ParsedToolCall {
                            id: "send-call".to_string(),
                            name: "danger_send".to_string(),
                            arguments: serde_json::json!({ "message": "hello" }),
                        }],
                        completed_results: vec![],
                    },
                ])),
                LoopInput::Resume { .. } => Ok(stream::iter(vec![
                    remi_agentloop::types::AgentEvent::TextDelta(
                        "resumed after denial".to_string(),
                    ),
                    remi_agentloop::types::AgentEvent::Done,
                ])),
                LoopInput::Cancel { .. } => {
                    Ok(stream::iter(vec![remi_agentloop::types::AgentEvent::Done]))
                }
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
            _ctx: &ToolContext,
        ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
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

    struct SteerDuringToolInnerAgent;

    impl Agent for SteerDuringToolInnerAgent {
        type Request = LoopInput;
        type Response = remi_agentloop::types::AgentEvent;
        type Error = AgentError;

        async fn chat(
            &self,
            req: Self::Request,
        ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
            match req {
                LoopInput::Start {
                    content,
                    history,
                    extra_tools,
                    ..
                } if content.text_content().contains("while-tool steer") => {
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
                LoopInput::Cancel { .. } => {
                    Ok(stream::iter(vec![remi_agentloop::types::AgentEvent::Done]))
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
            _ctx: &ToolContext,
        ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
            self.queue.push(CoreSteerInput {
                id: "queued-while-tool".to_string(),
                content: Content::text("while-tool steer"),
                preview: "while-tool steer".to_string(),
                message_metadata: None,
                user_name: None,
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

    #[tokio::test]
    async fn cat_agent_collects_parallel_tool_streams_end_to_end() {
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
    async fn cat_agent_injects_steer_after_tool_results_at_tool_gap() {
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
        };

        let events = agent
            .stream_with_input_and_options(
                LoopInput::start("start"),
                CoreStreamOptions::new().with_steer(queue),
            )
            .collect::<Vec<_>>()
            .await;

        assert!(
            events
                .iter()
                .any(|event| matches!(event, CatEvent::SteerInjected(event) if event.count == 1)),
            "{events:#?}"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, CatEvent::Text(text) if text == "steered")));
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
    async fn supervisor_run_stops_after_bounded_tool_rounds() {
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
