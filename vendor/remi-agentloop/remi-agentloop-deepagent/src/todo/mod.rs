//! TodoLayer — wraps any `Agent<Request=LoopInput, Response=DeepAgentEvent>` and
//! adds five todo management tools to it using the ExternalToolAgent pattern.
//!
//! The tools (`todo__add`, `todo__list`, `todo__complete`, `todo__update`,
//! `todo__remove`) are injected as `extra_tools` in each `LoopInput::Start`.
//! When the model calls one of them the `NeedToolExecution` event is caught
//! here, the tools are executed, `TodoEvent`s are emitted, and execution
//! resumes automatically — the consumer never sees the interruption.

pub mod tools;

use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_core::agent::Agent;
use remi_core::error::AgentError;
use remi_core::state::AgentState;
use remi_core::tool::{
    registry::{DefaultToolRegistry, ToolRegistry},
    ToolContext, ToolDefinition, ToolDefinitionContext, ToolOutput,
};
use remi_core::types::{AgentEvent, LoopInput, ParsedToolCall, ToolCallOutcome};
use std::collections::HashMap;

use crate::events::{DeepAgentEvent, TodoEvent};
use tools::{TodoAddTool, TodoCompleteTool, TodoListTool, TodoRemoveTool, TodoUpdateTool};

// ── TodoAgent ─────────────────────────────────────────────────────────────────

/// Wraps an inner agent and adds todo tools.
pub struct TodoAgent<A> {
    inner: A,
    tools: DefaultToolRegistry,
}

impl<A> TodoAgent<A> {
    pub fn new(inner: A) -> Self {
        let mut tools = DefaultToolRegistry::new();
        tools.register(TodoAddTool);
        tools.register(TodoListTool);
        tools.register(TodoCompleteTool);
        tools.register(TodoUpdateTool);
        tools.register(TodoRemoveTool);
        Self { inner, tools }
    }

    fn tool_definitions(
        &self,
        metadata: Option<serde_json::Value>,
        user_state: Option<serde_json::Value>,
    ) -> Vec<ToolDefinition> {
        let ctx = ToolDefinitionContext {
            metadata,
            user_state: user_state.unwrap_or(serde_json::Value::Null),
            ..ToolDefinitionContext::default()
        };
        self.tools.definitions_with_context(&ctx)
    }
}

// ── Agent impl ────────────────────────────────────────────────────────────────

impl<A> Agent for TodoAgent<A>
where
    A: Agent<Request = LoopInput, Response = DeepAgentEvent, Error = AgentError>,
{
    type Request = LoopInput;
    type Response = DeepAgentEvent;
    type Error = AgentError;

    async fn chat(
        &self,
        input: LoopInput,
    ) -> Result<impl Stream<Item = DeepAgentEvent>, AgentError> {
        // Inject our tool definitions into LoopInput::Start
        let input = match input {
            LoopInput::Start {
                content,
                history,
                mut extra_tools,
                model,
                temperature,
                max_tokens,
                metadata,
                message_metadata,
                user_name,
                user_state,
            } => {
                extra_tools.extend(self.tool_definitions(metadata.clone(), user_state.clone()));
                LoopInput::Start {
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
                }
            }
            other => other,
        };

        let stream = self.inner.chat(input).await?;
        let tools = &self.tools;

        Ok(todo_intercept_stream(stream, tools))
    }
}

// ── Stream interceptor ────────────────────────────────────────────────────────

fn todo_intercept_stream<'a, S>(
    stream: S,
    tools: &'a DefaultToolRegistry,
) -> impl Stream<Item = DeepAgentEvent> + 'a
where
    S: Stream<Item = DeepAgentEvent> + 'a,
{
    stream! {
        let mut current_input: Option<LoopInput> = None;
        let mut stream = std::pin::pin!(stream);

        loop {
            // If we have a resume input, restart the stream from inner.
            // We can't do that here since we don't hold &self.inner.
            // Instead, we yield the NeedToolExecution event back up if it
            // contains non-todo calls; for pure todo calls we must signal
            // the caller to re-invoke. Since we're a stream combinator we
            // handle resumption inline by building tool results and yielding
            // a special re-run. See note below.

            while let Some(event) = stream.next().await {
                match event {
                    DeepAgentEvent::Agent(AgentEvent::NeedToolExecution {
                        state,
                        tool_calls,
                        completed_results,
                    }) => {
                        // Partition calls: ours vs external
                        let (mine, external): (Vec<_>, Vec<_>) =
                            tool_calls.iter().cloned().partition(|tc| tools.contains(&tc.name));

                        // Execute our todo tools
                        let tool_ctx = make_tool_ctx(&state);
                        let resume_map = HashMap::new();
                        let mut all_outcomes: Vec<ToolCallOutcome> = completed_results;

                        if !mine.is_empty() {
                            let results =
                                tools.execute_parallel(&mine, &resume_map, &tool_ctx).await;

                            for (call_id, result) in results {
                                let tc = mine.iter().find(|t| t.id == call_id).unwrap();

                                // Emit TodoEvent for mutations
                                if let Some(ev) = make_todo_event(tc, &tool_ctx) {
                                    yield DeepAgentEvent::Todo(ev);
                                }

                                let result_str = match result {
                                    Err(e) => format!("error: {e}"),
                                    Ok(remi_core::tool::ToolResult::Interrupt(_)) => {
                                        "interrupted".to_string()
                                    }
                                    Ok(remi_core::tool::ToolResult::Output(mut stream)) => {
                                        let mut last = String::new();
                                        while let Some(out) = stream.next().await {
                                            if let ToolOutput::Result(c) = out {
                                                last = c.text_content();
                                            }
                                        }
                                        last
                                    }
                                };
                                all_outcomes.push(ToolCallOutcome::Result {
                                    tool_call_id: call_id,
                                    tool_name: tc.name.clone(),
                                    content: remi_core::types::Content::text(result_str),
                                });
                            }
                        }

                        // If there are still external calls, yield upward
                        if !external.is_empty() {
                            yield DeepAgentEvent::Agent(AgentEvent::NeedToolExecution {
                                state,
                                tool_calls: external,
                                completed_results: all_outcomes,
                            });
                            return;
                        }

                        // All calls were ours — store resume input and break inner to re-run
                        current_input = Some(LoopInput::Resume { state, results: all_outcomes });
                        break;
                    }
                    other => yield other,
                }
            }

            // If we have a resume, we need to re-drive the inner agent.
            // But since we hold a &A borrow that is tied to the outer chat()
            // call, we cannot call inner.chat() again here in a stream! macro.
            //
            // Solution: yield a sentinel NeedToolExecution with empty externals
            // so the outer DeepAgent loop can restart. Actually the cleanest
            // fix is to not use stream! but instead own the handle. We restructure:
            // this stream simply signals "needs resume" as a special internal event,
            // but that leaks implementation. For now we just stop — the caller
            // (outer DeepAgent) drives the loop below.
            //
            // In practice TodoAgent is wrapped by SkillAgent which is wrapped by
            // the DeepAgent run_loop, which owns the agent and can recall .chat().

            // No more events and no pending resume — we're done.
            if current_input.is_none() {
                return;
            }

            // We have a resume. Signal the need to restart by yielding a special
            // NeedToolExecution with no external calls and the resume state.
            // The DeepAgent::run_loop checks for this and re-calls chat().
            let resume = current_input.take().unwrap();
            if let LoopInput::Resume { state, results } = resume {
                yield DeepAgentEvent::Agent(AgentEvent::NeedToolExecution {
                    state,
                    tool_calls: vec![],        // no externals — signals "please resume me"
                    completed_results: results,
                });
            }
            return;
        }
    }
}

fn make_tool_ctx(state: &AgentState) -> ToolContext {
    let user_state = std::sync::Arc::new(std::sync::RwLock::new(state.user_state.clone()));
    ToolContext {
        config: remi_core::config::AgentConfig::default(),
        thread_id: Some(state.thread_id.clone()),
        run_id: state.run_id.clone(),
        metadata: state.config.metadata.clone(),
        cancel: None,
        user_state,
    }
}

/// Derive a `TodoEvent` from a just-executed todo tool call if it mutates state.
fn make_todo_event(tc: &ParsedToolCall, ctx: &ToolContext) -> Option<TodoEvent> {
    match tc.name.as_str() {
        "todo__add" => {
            let content = tc.arguments["content"].as_str()?.to_string();
            // Read the freshly-written ID from user_state
            let us = ctx.user_state.read().unwrap();
            let todos: Vec<crate::todo::tools::TodoItem> =
                serde_json::from_value(us["__todos"].clone()).unwrap_or_default();
            let id = todos.iter().map(|t| t.id).max().unwrap_or(0);
            Some(TodoEvent::Added { id, content })
        }
        "todo__complete" => {
            let id = tc.arguments["id"].as_u64()?;
            Some(TodoEvent::Completed { id })
        }
        "todo__update" => {
            let id = tc.arguments["id"].as_u64()?;
            let content = tc.arguments["content"].as_str()?.to_string();
            Some(TodoEvent::Updated { id, content })
        }
        "todo__remove" => {
            let id = tc.arguments["id"].as_u64()?;
            Some(TodoEvent::Removed { id })
        }
        _ => None,
    }
}

// ── TodoLayer ─────────────────────────────────────────────────────────────────

/// Layer adapter — apply with `agent.layer(TodoLayer)`.
pub struct TodoLayer;

impl<A> remi_core::agent::Layer<A> for TodoLayer
where
    A: Agent<Request = LoopInput, Response = DeepAgentEvent, Error = AgentError>,
{
    type Output = TodoAgent<A>;
    fn layer(self, inner: A) -> Self::Output {
        TodoAgent::new(inner)
    }
}
