//! Composable agent loop: step() + tool execution cycle.
//!
//! `AgentLoop` is the core engine that repeatedly calls [`step()`] and
//! executes tools until the model conversation is done or interrupted.
//!
//! It is **pure** — no memory persistence, no run lifecycle.  Those
//! concerns are handled by the outer [`BuiltAgent`](crate::builder::BuiltAgent)
//! layer which observes the [`AgentEvent::Checkpoint`] stream and
//! persists to a [`CheckpointStore`] / [`ContextStore`].
//!
//! ```text
//! chat() / chat_in_thread()
//!   └── run_loop()          ← memory + run lifecycle
//!         └── agent_loop()  ← step + tools + tracing  (this module)
//!               └── step()  ← single model call
//! ```
//!
//! `AgentLoop` implements the [`Agent`] trait, so it can be freely
//! composed with adapters (Logging, Retry, TracingLayer, etc.).

#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use async_stream::stream;
use futures::{Stream, StreamExt};

use crate::checkpoint::{Checkpoint, CheckpointStatus};
use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::model::ChatModel;
use crate::state::{step, Action, AgentState, StepConfig, StepEvent};
use crate::tool::registry::ToolRegistry;
use crate::tool::{ToolDefinitionContext, ToolOutput, ToolResult};
use crate::tracing::{
    DynTracer, ExternalToolResultTrace, InterruptTrace, ModelEndTrace, ModelStartTrace,
    ResumeTrace, RunEndTrace, RunStartTrace, RunStatus, ToolCallTrace, ToolEndTrace,
    ToolExecutionHandoffTrace, ToolOutcomeTrace, ToolStartTrace, TurnStartTrace,
};
use crate::types::{AgentEvent, Content, InterruptInfo, Message, ParsedToolCall, ToolCallOutcome};

// ── AgentLoop ─────────────────────────────────────────────────────────────────

/// Composable step + tool execution loop.
///
/// Yields [`AgentEvent`]s including [`AgentEvent::Checkpoint`] at key
/// lifecycle boundaries (so outer layers can persist and resume).
pub struct AgentLoop<M: ChatModel> {
    pub(crate) model: M,
    pub(crate) tools: Box<dyn ToolRegistry>,
    pub(crate) config: AgentConfig,
    pub(crate) tracer: Option<Box<dyn DynTracer>>,
    pub(crate) system_prompt: String,
    pub(crate) max_turns: usize,
}

impl<M: ChatModel> AgentLoop<M> {
    fn outcome_trace(outcome: &ToolCallOutcome) -> ToolOutcomeTrace {
        match outcome {
            ToolCallOutcome::Result {
                tool_call_id,
                tool_name,
                content,
            } => ToolOutcomeTrace {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                result: Some(content.text_content()),
                error: None,
            },
            ToolCallOutcome::Error {
                tool_call_id,
                tool_name,
                error,
            } => ToolOutcomeTrace {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                result: None,
                error: Some(error.clone()),
            },
        }
    }

    fn tool_definition_context(state: &AgentState) -> ToolDefinitionContext {
        ToolDefinitionContext {
            thread_id: Some(state.thread_id.clone()),
            run_id: Some(state.run_id.clone()),
            metadata: state.config.metadata.clone(),
            user_state: state.user_state.clone(),
        }
    }

    fn refresh_local_tool_definitions(state: &mut AgentState, tools: &dyn ToolRegistry) {
        let ctx = Self::tool_definition_context(state);
        let local_defs = tools.definitions_with_context(&ctx);
        let external_defs: Vec<_> = state
            .tool_definitions
            .iter()
            .filter(|d| !tools.contains(&d.function.name))
            .cloned()
            .collect();
        state.tool_definitions = local_defs;
        state.tool_definitions.extend(external_defs);
    }

    /// Build the initial [`AgentState`] for a new run.
    ///
    /// The returned state has `tool_definitions` populated from the
    /// registry.  If you need to add external tool definitions (for
    /// outer-layer execution), append them to `state.tool_definitions`
    /// before calling [`run()`](Self::run).
    pub fn build_state(&self, messages: Vec<Message>) -> AgentState {
        let model_name = self.config.model.clone().unwrap_or_default();
        let extra_body = match &self.config.extra {
            serde_json::Value::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        };
        let mut state = AgentState::new(StepConfig {
            model: model_name,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            metadata: None,
            rate_limit_retry: self.config.rate_limit_retry.clone(),
            extra_body,
        });
        state.system_prompt = if self.system_prompt.is_empty() {
            None
        } else {
            Some(self.system_prompt.clone())
        };
        state.tool_definitions = self
            .tools
            .definitions_with_context(&Self::tool_definition_context(&state));
        state.messages = messages;
        state
    }

    /// Run the step + tool execution loop.
    ///
    /// This is the composable core.  It does **not** persist messages
    /// or manage run lifecycle — it only calls `step()`, executes tools,
    /// and yields events.
    ///
    /// Callers that need persistence should observe
    /// [`AgentEvent::Checkpoint`] in the returned stream.
    pub fn run<'a>(
        &'a self,
        initial_state: AgentState,
        initial_action: Action,
        emit_run_start: bool,
    ) -> impl Stream<Item = AgentEvent> + 'a {
        let max_turns = self.max_turns;
        let model = &self.model;
        let tools = self.tools.as_ref();
        let tracer = self.tracer.as_deref();

        stream! {
            let run_start_time = Instant::now();
            let mut total_prompt_tokens = 0u32;
            let mut total_completion_tokens = 0u32;

            let mut state = initial_state;
            let mut action = initial_action;
            let mut turn = if state.turn == 0 { 1usize } else { state.turn };
            state.turn = turn;

            // Monotonic checkpoint sequence counter
            #[allow(unused_assignments)]
            let mut checkpoint_seq: u64 = 0;

            // Capture run_id for tracing (stable across turns)
            let run_id = state.run_id.clone();
            let model_name = state.config.model.clone();

            if emit_run_start {
                if let Some(t) = tracer {
                    // Build the full input message list for the trace —
                    // include the upcoming user message so the trace shows
                    // what the model will actually receive.
                    let mut trace_input = state.messages.clone();
                    match &action {
                        Action::UserMessage(text) => {
                            trace_input.push(Message::user(text));
                        }
                        Action::UserContent { ref content, .. } => {
                            trace_input.push(Message {
                                id: crate::types::MessageId::new(),
                                role: crate::types::Role::User,
                                content: content.clone(),
                                tool_calls: None,
                                tool_call_id: None,
                                name: None,
                                reasoning_content: None,
                                metadata: None,
                            });
                        }
                        _ => {}
                    }
                    t.on_run_start(&RunStartTrace {
                        thread_id: Some(state.thread_id.clone()),
                        run_id: run_id.clone(),
                        model: model_name.clone(),
                        system_prompt: state.system_prompt.clone(),
                        input_messages: trace_input,
                        metadata: state.config.metadata.clone(),
                        timestamp: chrono::Utc::now(),
                    }).await;
                }
                yield AgentEvent::RunStart {
                    thread_id: state.thread_id.clone(),
                    run_id: state.run_id.clone(),
                    metadata: state.config.metadata.clone(),
                };
            }

            loop {
                if turn > max_turns {
                    if let Some(t) = tracer {
                        t.on_run_end(&RunEndTrace {
                            run_id: run_id.clone(),
                            status: RunStatus::MaxTurnsExceeded,
                            output_messages: vec![],
                            total_turns: turn - 1,
                            total_prompt_tokens,
                            total_completion_tokens,
                            duration: run_start_time.elapsed(),
                            error: Some(format!("max turns exceeded: {max_turns}")),
                            timestamp: chrono::Utc::now(),
                        }).await;
                    }
                    yield AgentEvent::Error(AgentError::MaxTurnsExceeded { max: max_turns });
                    return;
                }

                Self::refresh_local_tool_definitions(&mut state, tools);

                // ── Model start trace ─────────────────────────────────
                let tool_names: Vec<String> = state.tool_definitions.iter()
                    .map(|td| td.function.name.clone())
                    .collect();
                let model_call_index = state.model_call_seq;
                state.model_call_seq += 1;

                if let Some(t) = tracer {
                    t.on_model_start(&ModelStartTrace {
                        run_id: run_id.clone(),
                        turn,
                        call_index: model_call_index,
                        model: model_name.clone(),
                        messages: state.messages.clone(),
                        tools: tool_names,
                        timestamp: chrono::Utc::now(),
                    }).await;
                }

                let model_start = Instant::now();

                // ── Run one step ──────────────────────────────────────
                let step_stream = step(state, action, model);
                let mut step_stream = std::pin::pin!(step_stream);

                let mut next_state: Option<AgentState> = None;
                let mut pending_tool_calls: Option<Vec<ParsedToolCall>> = None;
                let mut response_text = String::new();
                let mut step_prompt_tokens = 0u32;
                let mut step_completion_tokens = 0u32;

                while let Some(event) = step_stream.next().await {
                    match event {
                        StepEvent::TextDelta(text) => {
                            response_text.push_str(&text);
                            yield AgentEvent::TextDelta(text);
                        }
                        StepEvent::ReasoningStart => {
                            yield AgentEvent::ThinkingStart;
                        }
                        StepEvent::ReasoningEnd { content } => {
                            yield AgentEvent::ThinkingEnd { content };
                        }
                        StepEvent::ToolCallStart { id, name } => {
                            yield AgentEvent::ToolCallStart { id, name };
                        }
                        StepEvent::ToolCallArgumentsDelta { id, delta } => {
                            yield AgentEvent::ToolCallArgumentsDelta { id, delta };
                        }
                        StepEvent::Usage { prompt_tokens, completion_tokens } => {
                            step_prompt_tokens = prompt_tokens;
                            step_completion_tokens = completion_tokens;
                            total_prompt_tokens += prompt_tokens;
                            total_completion_tokens += completion_tokens;
                            yield AgentEvent::Usage { prompt_tokens, completion_tokens };
                        }
                        StepEvent::Done { state: s } => {
                            next_state = Some(s);
                        }
                        StepEvent::NeedToolExecution { state: s, tool_calls } => {
                            pending_tool_calls = Some(tool_calls);
                            next_state = Some(s);
                        }
                        StepEvent::Error { state: _, error } => {
                            if let Some(t) = tracer {
                                t.on_model_end(&ModelEndTrace {
                                    run_id: run_id.clone(),
                                    turn,
                                    call_index: model_call_index,
                                    response_text: None,
                                    tool_calls: vec![],
                                    prompt_tokens: step_prompt_tokens,
                                    completion_tokens: step_completion_tokens,
                                    duration: model_start.elapsed(),
                                    timestamp: chrono::Utc::now(),
                                }).await;
                                t.on_run_end(&RunEndTrace {
                                    run_id: run_id.clone(),
                                    status: RunStatus::Error,
                                    output_messages: vec![],
                                    total_turns: turn,
                                    total_prompt_tokens,
                                    total_completion_tokens,
                                    duration: run_start_time.elapsed(),
                                    error: Some(error.to_string()),
                                    timestamp: chrono::Utc::now(),
                                }).await;
                            }
                            yield AgentEvent::Error(error);
                            return;
                        }
                    }
                }

                state = match next_state {
                    Some(s) => s,
                    None => {
                        yield AgentEvent::Error(AgentError::other("step() ended without terminal event"));
                        return;
                    }
                };

                // ── Model end trace ───────────────────────────────────
                let model_duration = model_start.elapsed();
                let tc_traces: Vec<ToolCallTrace> = pending_tool_calls.as_ref()
                    .map(|tcs| tcs.iter().map(|tc| ToolCallTrace {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        result: None,
                        interrupted: false,
                        duration: std::time::Duration::ZERO,
                    }).collect())
                    .unwrap_or_default();

                if let Some(t) = tracer {
                    t.on_model_end(&ModelEndTrace {
                        run_id: run_id.clone(),
                        turn,
                        call_index: model_call_index,
                        response_text: if response_text.is_empty() { None } else { Some(response_text.clone()) },
                        tool_calls: tc_traces,
                        prompt_tokens: step_prompt_tokens,
                        completion_tokens: step_completion_tokens,
                        duration: model_duration,
                        timestamp: chrono::Utc::now(),
                    }).await;
                }

                match pending_tool_calls {
                    None => {
                        // ── Done — no tool calls ──────────────────────
                        if let Some(t) = tracer {
                            t.on_run_end(&RunEndTrace {
                                run_id: run_id.clone(),
                                status: RunStatus::Completed,
                                output_messages: state.messages.clone(),
                                total_turns: turn,
                                total_prompt_tokens,
                                total_completion_tokens,
                                duration: run_start_time.elapsed(),
                                error: None,
                                timestamp: chrono::Utc::now(),
                            }).await;
                        }
                        // ── Checkpoint: RunDone ───────────────────────
                        yield AgentEvent::Checkpoint(Checkpoint::new(
                            state.thread_id.clone(),
                            run_id.clone(),
                            state.clone(),
                            None,
                            turn,
                            CheckpointStatus::RunDone,
                            checkpoint_seq,
                        ));
                        yield AgentEvent::Done;
                        return;
                    }
                    Some(tool_calls) => {
                        // ── Split: local vs external tool calls ───────
                        let (local_calls, external_calls): (Vec<_>, Vec<_>) =
                            tool_calls.iter().cloned().partition(|tc| tools.contains(&tc.name));

                        // ── Execute local tools ───────────────────────
                        let resume_map = std::collections::HashMap::new();

                        let shared_user_state = std::sync::Arc::new(
                            std::sync::RwLock::new(state.user_state.clone()),
                        );

                        let tool_ctx = crate::tool::ToolContext {
                            config: self.config.clone(),
                            thread_id: Some(state.thread_id.clone()),
                            run_id: run_id.clone(),
                            metadata: state.config.metadata.clone(),
                            cancel: None,
                            user_state: shared_user_state.clone(),
                        };

                        if let Some(t) = tracer {
                            for tc in &local_calls {
                                t.on_tool_start(&ToolStartTrace {
                                    run_id: run_id.clone(),
                                    turn,
                                    tool_call_id: tc.id.clone(),
                                    tool_name: tc.name.clone(),
                                    arguments: tc.arguments.clone(),
                                    timestamp: chrono::Utc::now(),
                                }).await;
                            }
                        }

                        let tool_exec_start = Instant::now();

                        let mut outcomes: Vec<ToolCallOutcome> = Vec::new();
                        let mut pending_interrupts: Vec<InterruptInfo> = Vec::new();

                        if !local_calls.is_empty() {
                            let tool_results = tools.execute_parallel(&local_calls, &resume_map, &tool_ctx).await;

                            for (tool_call_id, tool_result) in tool_results {
                                let tc = local_calls.iter().find(|p| p.id == tool_call_id).unwrap();

                                match tool_result {
                                    Err(e) => {
                                        let msg = e.to_string();
                                        if let Some(t) = tracer {
                                            t.on_tool_end(&ToolEndTrace {
                                                run_id: run_id.clone(),
                                                turn,
                                                tool_call_id: tool_call_id.clone(),
                                                tool_name: tc.name.clone(),
                                                result: Some(msg.clone()),
                                                interrupted: false,
                                                error: Some(msg.clone()),
                                                duration: tool_exec_start.elapsed(),
                                                timestamp: chrono::Utc::now(),
                                            }).await;
                                        }
                                        yield AgentEvent::Error(e);
                                        outcomes.push(ToolCallOutcome::Error {
                                            tool_call_id,
                                            tool_name: tc.name.clone(),
                                            error: msg,
                                        });
                                    }
                                    Ok(ToolResult::Interrupt(req)) => {
                                        if let Some(t) = tracer {
                                            t.on_tool_end(&ToolEndTrace {
                                                run_id: run_id.clone(),
                                                turn,
                                                tool_call_id: tool_call_id.clone(),
                                                tool_name: tc.name.clone(),
                                                result: None,
                                                interrupted: true,
                                                error: None,
                                                duration: tool_exec_start.elapsed(),
                                                timestamp: chrono::Utc::now(),
                                            }).await;
                                        }
                                        pending_interrupts.push(InterruptInfo {
                                            interrupt_id: req.interrupt_id,
                                            tool_call_id: tool_call_id.clone(),
                                            tool_name: tc.name.clone(),
                                            kind: req.kind,
                                            data: req.data,
                                        });
                                    }
                                    Ok(ToolResult::Output(mut tool_stream)) => {
                                        let mut last_result: Option<Content> = None;
                                        while let Some(output) = tool_stream.next().await {
                                            match output {
                                                ToolOutput::Delta(delta) => {
                                                    yield AgentEvent::ToolDelta {
                                                        id: tool_call_id.clone(),
                                                        name: tc.name.clone(),
                                                        delta,
                                                    };
                                                }
                                                ToolOutput::SubSession(mut event) => {
                                                    if event.parent_tool_call_id.is_empty() {
                                                        event.parent_tool_call_id = tool_call_id.clone();
                                                    }
                                                    yield AgentEvent::SubSession(event);
                                                }
                                                ToolOutput::Result(content) => {
                                                    yield AgentEvent::ToolResult {
                                                        id: tool_call_id.clone(),
                                                        name: tc.name.clone(),
                                                        result: content.text_content(),
                                                    };
                                                    last_result = Some(content);
                                                }
                                            }
                                        }
                                        if let Some(content) = &last_result {
                                            if let Some(t) = tracer {
                                                t.on_tool_end(&ToolEndTrace {
                                                    run_id: run_id.clone(),
                                                    turn,
                                                    tool_call_id: tool_call_id.clone(),
                                                    tool_name: tc.name.clone(),
                                                    result: Some(content.text_content()),
                                                    interrupted: false,
                                                    error: None,
                                                    duration: tool_exec_start.elapsed(),
                                                    timestamp: chrono::Utc::now(),
                                                }).await;
                                            }
                                            outcomes.push(ToolCallOutcome::Result {
                                                tool_call_id,
                                                tool_name: tc.name.clone(),
                                                content: last_result.unwrap(),
                                            });
                                        }
                                    }
                                }
                            }
                        }

                        // ── Handle interrupts ─────────────────────────
                        if !pending_interrupts.is_empty() {
                            if let Some(t) = tracer {
                                t.on_interrupt(&InterruptTrace {
                                    run_id: run_id.clone(),
                                    interrupts: pending_interrupts.clone(),
                                    timestamp: chrono::Utc::now(),
                                }).await;
                                t.on_run_end(&RunEndTrace {
                                    run_id: run_id.clone(),
                                    status: RunStatus::Interrupted,
                                    output_messages: state.messages.clone(),
                                    total_turns: turn,
                                    total_prompt_tokens,
                                    total_completion_tokens,
                                    duration: run_start_time.elapsed(),
                                    error: None,
                                    timestamp: chrono::Utc::now(),
                                }).await;
                            }
                            // ── Checkpoint: Interrupted ───────────────
                            yield AgentEvent::Checkpoint(Checkpoint::new(
                                state.thread_id.clone(),
                                run_id.clone(),
                                state.clone(),
                                None, // resume via ChatInput::Resume, not via Action
                                turn,
                                CheckpointStatus::Interrupted,
                                checkpoint_seq,
                            ));
                            yield AgentEvent::Interrupt { interrupts: pending_interrupts };
                            return;
                        }

                        // ── External tool calls: yield outward ────────
                        if !external_calls.is_empty() {
                            if let Some(t) = tracer {
                                t.on_tool_execution_handoff(&ToolExecutionHandoffTrace {
                                    run_id: run_id.clone(),
                                    turn,
                                    tool_calls: external_calls.iter().map(|tc| ToolCallTrace {
                                        id: tc.id.clone(),
                                        name: tc.name.clone(),
                                        arguments: tc.arguments.clone(),
                                        result: None,
                                        interrupted: false,
                                        duration: std::time::Duration::ZERO,
                                    }).collect(),
                                    completed_results: outcomes.iter().map(Self::outcome_trace).collect(),
                                    timestamp: chrono::Utc::now(),
                                }).await;
                            }
                            // Write back user_state before yielding
                            state.user_state = shared_user_state.read().unwrap().clone();
                            // ── Checkpoint: AwaitingToolExecution ──────
                            yield AgentEvent::Checkpoint(Checkpoint::new(
                                state.thread_id.clone(),
                                run_id.clone(),
                                state.clone(),
                                None, // resume via LoopInput::Resume externally
                                turn,
                                CheckpointStatus::AwaitingToolExecution,
                                checkpoint_seq,
                            ));
                            yield AgentEvent::NeedToolExecution {
                                state,
                                tool_calls: external_calls,
                                completed_results: outcomes,
                            };
                            return;
                        }

                        // ── All tools local, continue loop ────────────
                        state.user_state = shared_user_state.read().unwrap().clone();

                        // Refresh local tool defs while preserving
                        // externally-injected definitions (those not in
                        // this registry).  This allows outer layers to
                        // inject additional tool defs that survive across turns.
                        Self::refresh_local_tool_definitions(&mut state, tools);

                        // ── Checkpoint: ToolsExecuted ─────────────────
                        let next_action = Action::ToolResults(outcomes);
                        yield AgentEvent::Checkpoint(Checkpoint::new(
                            state.thread_id.clone(),
                            run_id.clone(),
                            state.clone(),
                            Some(next_action.clone()),
                            turn,
                            CheckpointStatus::ToolsExecuted,
                            checkpoint_seq,
                        ));
                        checkpoint_seq += 1;

                        turn += 1;
                        state.turn = turn;
                        if let Some(t) = tracer {
                            t.on_turn_start(&TurnStartTrace {
                                run_id: run_id.clone(),
                                turn,
                                timestamp: chrono::Utc::now(),
                            }).await;
                        }
                        yield AgentEvent::TurnStart { turn };
                        action = next_action;
                    }
                }
            }
        }
    }
}

// ── Agent impl ────────────────────────────────────────────────────────────────

impl<M: ChatModel> crate::agent::Agent for AgentLoop<M> {
    type Request = crate::types::LoopInput;
    type Response = AgentEvent;
    type Error = AgentError;

    fn chat(
        &self,
        input: crate::types::LoopInput,
    ) -> impl std::future::Future<Output = Result<impl Stream<Item = AgentEvent>, AgentError>> {
        async move {
            let stream: std::pin::Pin<Box<dyn Stream<Item = AgentEvent> + '_>> = match input {
                crate::types::LoopInput::Start {
                    content,
                    history,
                    extra_tools,
                    model,
                    temperature,
                    max_tokens,
                    metadata,
                    message_metadata,
                    user_name,
                    user_state,
                } => {
                    let mut state = self.build_state(history);
                    if !extra_tools.is_empty() {
                        state.tool_definitions.extend(extra_tools);
                    }
                    // Apply per-request overrides
                    if let Some(m) = model {
                        state.config.model = m;
                    }
                    if let Some(t) = temperature {
                        state.config.temperature = Some(t);
                    }
                    if let Some(n) = max_tokens {
                        state.config.max_tokens = Some(n);
                    }
                    if let Some(v) = metadata {
                        state.config.metadata = Some(v);
                    }
                    if let Some(us) = user_state {
                        state.user_state = us;
                    }
                    let action = Action::UserContent {
                        content,
                        message_metadata,
                        user_name,
                    };
                    Box::pin(self.run(state, action, true))
                }
                crate::types::LoopInput::Resume { state, results } => {
                    let outcomes = results.iter().map(Self::outcome_trace).collect::<Vec<_>>();
                    // Emit on_resume before the stream starts so LangSmith
                    // (and any other tracer) can reactivate the run record.
                    if let Some(t) = self.tracer.as_deref() {
                        t.on_resume(&ResumeTrace {
                            run_id: state.run_id.clone(),
                            payloads_count: results.len(),
                            outcomes: outcomes.clone(),
                            timestamp: chrono::Utc::now(),
                        })
                        .await;

                        for outcome in &outcomes {
                            t.on_external_tool_result(&ExternalToolResultTrace {
                                run_id: state.run_id.clone(),
                                tool_call_id: outcome.tool_call_id.clone(),
                                tool_name: outcome.tool_name.clone(),
                                result: outcome.result.clone(),
                                error: outcome.error.clone(),
                                timestamp: chrono::Utc::now(),
                            })
                            .await;
                        }
                    }
                    Box::pin(self.run(state, Action::ToolResults(results), false))
                }
                crate::types::LoopInput::Cancel { mut state } => {
                    state.phase = crate::state::AgentPhase::Done;
                    Box::pin(self.cancel_run(state))
                }
            };
            Ok(stream)
        }
    }
}

impl<M: ChatModel> AgentLoop<M> {
    /// Produce a `Cancelled` checkpoint and event for the given state.
    ///
    /// This is a lightweight path that does not call `step()`.
    pub fn cancel_run(&self, state: AgentState) -> impl Stream<Item = AgentEvent> + '_ {
        stream! {
            yield AgentEvent::Checkpoint(Checkpoint::new(
                state.thread_id.clone(),
                state.run_id.clone(),
                state,
                None,
                0,
                CheckpointStatus::Cancelled,
                0,
            ));
            yield AgentEvent::Cancelled;
        }
    }

    /// Flush any buffered tracing I/O (e.g. pending LangSmith HTTP calls).
    ///
    /// Call this after the agent event stream has been fully consumed and
    /// before the async runtime shuts down.
    pub async fn flush_tracer(&self) {
        if let Some(t) = self.tracer.as_deref() {
            t.on_flush().await;
        }
    }
}
