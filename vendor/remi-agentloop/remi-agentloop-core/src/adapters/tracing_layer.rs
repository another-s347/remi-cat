use async_stream::stream;
use futures::{Stream, StreamExt};
use std::future::Future;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use crate::agent::{Agent, Layer};
use crate::error::AgentError;
use crate::tracing::{
    ExternalToolResultTrace, ModelEndTrace, ResumeTrace, RunEndTrace, RunStartTrace, RunStatus,
    ToolCallTrace, ToolExecutionHandoffTrace, ToolOutcomeTrace, TurnStartTrace,
};
use crate::types::{AgentEvent, LoopInput, ToolCallOutcome};

// ── TracingLayer ──────────────────────────────────────────────────────────────

/// A [`Layer`] that wraps any `Agent<Response = AgentEvent>` with tracing.
///
/// This is useful for adding tracing to agents that don't use `BuiltAgent`
/// (e.g., `ProtocolAgent`, `HttpSseClient`, `WasmAgent`) or for adding
/// an additional tracer at the transport boundary.
///
/// For `BuiltAgent`, prefer using `.tracer()` on the builder, which
/// provides richer per-tool-call tracing.
///
/// ```ignore
/// use remi_agentloop::prelude::*;
/// use remi_agentloop::adapters::tracing_layer::TracingLayer;
/// use remi_agentloop::tracing::stdout::StdoutTracer;
///
/// let agent = some_agent.layer(TracingLayer::new(StdoutTracer));
/// ```
pub struct TracingLayer<T> {
    tracer: T,
}

impl<T> TracingLayer<T> {
    pub fn new(tracer: T) -> Self {
        Self { tracer }
    }
}

impl<A, T> Layer<A> for TracingLayer<T>
where
    A: Agent<Request = crate::types::LoopInput, Response = AgentEvent, Error = AgentError>,
    T: crate::tracing::Tracer + Send + Sync + 'static,
{
    type Output = TracedAgent<A, T>;

    fn layer(self, inner: A) -> Self::Output {
        TracedAgent {
            inner,
            tracer: self.tracer,
        }
    }
}

// ── TracedAgent ───────────────────────────────────────────────────────────────

/// An agent wrapper that emits tracer events by observing the `AgentEvent` stream.
///
/// Created by [`TracingLayer`].
pub struct TracedAgent<A, T> {
    inner: A,
    tracer: T,
}

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

impl<A, T> Agent for TracedAgent<A, T>
where
    A: Agent<Request = crate::types::LoopInput, Response = AgentEvent, Error = AgentError>,
    T: crate::tracing::Tracer + Send + Sync,
{
    type Request = crate::types::LoopInput;
    type Response = AgentEvent;
    type Error = AgentError;

    fn chat(
        &self,
        req: crate::types::LoopInput,
    ) -> impl Future<Output = Result<impl Stream<Item = AgentEvent>, AgentError>> {
        async move {
            let initial_run_id = match &req {
                LoopInput::Resume { state, results } => {
                    let outcomes = results.iter().map(outcome_trace).collect::<Vec<_>>();
                    self.tracer
                        .on_resume(&ResumeTrace {
                            run_id: state.run_id.clone(),
                            payloads_count: results.len(),
                            outcomes: outcomes.clone(),
                            timestamp: chrono::Utc::now(),
                        })
                        .await;
                    for outcome in &outcomes {
                        self.tracer
                            .on_external_tool_result(&ExternalToolResultTrace {
                                run_id: state.run_id.clone(),
                                tool_call_id: outcome.tool_call_id.clone(),
                                tool_name: outcome.tool_name.clone(),
                                result: outcome.result.clone(),
                                error: outcome.error.clone(),
                                timestamp: chrono::Utc::now(),
                            })
                            .await;
                    }
                    state.run_id.clone()
                }
                LoopInput::Cancel { state } => state.run_id.clone(),
                LoopInput::Start { .. } => crate::types::RunId::new(),
            };

            let inner_stream = self.inner.chat(req).await?;

            Ok(stream! {
                let mut inner_stream = std::pin::pin!(inner_stream);

                let run_start_time = Instant::now();
                let mut run_id = initial_run_id;
                let mut turn = 1usize;
                let mut model_call_seq = 0usize;
                let mut total_prompt_tokens = 0u32;
                let mut total_completion_tokens = 0u32;

                while let Some(event) = inner_stream.next().await {
                    match &event {
                        AgentEvent::RunStart { run_id: rid, .. } => {
                            run_id = rid.clone();
                            self.tracer.on_run_start(&RunStartTrace {
                                thread_id: None,
                                run_id: run_id.clone(),
                                model: String::new(),
                                system_prompt: None,
                                input_messages: vec![],
                                metadata: None,
                                timestamp: chrono::Utc::now(),
                            }).await;
                        }
                        AgentEvent::TurnStart { turn: t } => {
                            turn = *t;
                            self.tracer.on_turn_start(&TurnStartTrace {
                                run_id: run_id.clone(),
                                turn,
                                timestamp: chrono::Utc::now(),
                            }).await;
                        }
                        AgentEvent::Usage { prompt_tokens, completion_tokens } => {
                            total_prompt_tokens += prompt_tokens;
                            total_completion_tokens += completion_tokens;
                            // ModelEnd fires on Usage
                            self.tracer.on_model_end(&ModelEndTrace {
                                run_id: run_id.clone(),
                                turn,
                                call_index: model_call_seq,
                                response_text: None,
                                tool_calls: vec![],
                                prompt_tokens: *prompt_tokens,
                                completion_tokens: *completion_tokens,
                                duration: run_start_time.elapsed(),
                                timestamp: chrono::Utc::now(),
                            }).await;
                            model_call_seq += 1;
                        }
                        AgentEvent::Done => {
                            self.tracer.on_run_end(&RunEndTrace {
                                run_id: run_id.clone(),
                                status: RunStatus::Completed,
                                output_messages: vec![],
                                total_turns: turn,
                                total_prompt_tokens,
                                total_completion_tokens,
                                duration: run_start_time.elapsed(),
                                error: None,
                                timestamp: chrono::Utc::now(),
                            }).await;
                        }
                        AgentEvent::Error(e) => {
                            self.tracer.on_run_end(&RunEndTrace {
                                run_id: run_id.clone(),
                                status: RunStatus::Error,
                                output_messages: vec![],
                                total_turns: turn,
                                total_prompt_tokens,
                                total_completion_tokens,
                                duration: run_start_time.elapsed(),
                                error: Some(e.to_string()),
                                timestamp: chrono::Utc::now(),
                            }).await;
                        }
                        AgentEvent::NeedToolExecution {
                            state: _,
                            tool_calls,
                            completed_results,
                        } => {
                            self.tracer.on_tool_execution_handoff(&ToolExecutionHandoffTrace {
                                run_id: run_id.clone(),
                                turn,
                                tool_calls: tool_calls.iter().map(|tc| ToolCallTrace {
                                    id: tc.id.clone(),
                                    name: tc.name.clone(),
                                    arguments: tc.arguments.clone(),
                                    result: None,
                                    interrupted: false,
                                    duration: std::time::Duration::ZERO,
                                }).collect(),
                                completed_results: completed_results.iter().map(outcome_trace).collect(),
                                timestamp: chrono::Utc::now(),
                            }).await;
                        }
                        _ => {}
                    }
                    yield event;
                }
            })
        }
    }
}
