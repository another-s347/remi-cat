//! SkillLayer — wraps any `Agent<Request=LoopInput, Response=DeepAgentEvent>` and
//! adds four skill management tools using the ExternalToolAgent pattern.

pub mod store;
pub mod tools;

use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_core::agent::Agent;
use remi_core::error::AgentError;
use remi_core::tool::{
    registry::{DefaultToolRegistry, ToolRegistry},
    ToolContext, ToolDefinition, ToolDefinitionContext, ToolOutput,
};
use remi_core::types::{AgentEvent, Content, LoopInput, ParsedToolCall, ToolCallOutcome};
use std::collections::HashMap;
use std::sync::Arc;

use crate::events::{DeepAgentEvent, SkillEvent};
use store::SkillStore;
use tools::{SkillDeleteTool, SkillGetTool, SkillListTool, SkillSaveTool};

// ── SkillAgent ────────────────────────────────────────────────────────────────

pub struct SkillAgent<A, S> {
    inner: A,
    tools: DefaultToolRegistry,
    store: Arc<S>,
}

impl<A, S: SkillStore> SkillAgent<A, S> {
    pub fn new(inner: A, store: S) -> Self {
        let store = Arc::new(store);
        let mut tools = DefaultToolRegistry::new();
        tools.register(SkillSaveTool {
            store: Arc::clone(&store),
        });
        tools.register(SkillGetTool {
            store: Arc::clone(&store),
        });
        tools.register(SkillListTool {
            store: Arc::clone(&store),
        });
        tools.register(SkillDeleteTool {
            store: Arc::clone(&store),
        });
        Self {
            inner,
            tools,
            store,
        }
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

impl<A, S> Agent for SkillAgent<A, S>
where
    A: Agent<Request = LoopInput, Response = DeepAgentEvent, Error = AgentError>,
    S: SkillStore,
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

        Ok(skill_intercept_stream(stream, tools))
    }
}

// ── Stream interceptor ────────────────────────────────────────────────────────

fn skill_intercept_stream<'a, S>(
    stream: S,
    tools: &'a DefaultToolRegistry,
) -> impl Stream<Item = DeepAgentEvent> + 'a
where
    S: Stream<Item = DeepAgentEvent> + 'a,
{
    stream! {
        let mut stream = std::pin::pin!(stream);

        while let Some(event) = stream.next().await {
            match event {
                DeepAgentEvent::Agent(AgentEvent::NeedToolExecution {
                    state,
                    tool_calls,
                    completed_results,
                }) => {
                    let (mine, external): (Vec<_>, Vec<_>) =
                        tool_calls.iter().cloned().partition(|tc| tools.contains(&tc.name));

                    let tool_ctx = make_tool_ctx(&state);
                    let resume_map = HashMap::new();
                    let mut all_outcomes: Vec<ToolCallOutcome> = completed_results;

                    if !mine.is_empty() {
                        let results =
                            tools.execute_parallel(&mine, &resume_map, &tool_ctx).await;

                        for (call_id, result) in results {
                            let tc = mine.iter().find(|t| t.id == call_id).unwrap();

                            // Emit SkillEvent for mutations
                            if let Some(ev) = make_skill_event(tc, &tool_ctx) {
                                yield DeepAgentEvent::Skill(ev);
                            }

                            let result_str = match result {
                                Err(e) => format!("error: {e}"),
                                Ok(remi_core::tool::ToolResult::Interrupt(_)) => "interrupted".to_string(),
                                Ok(remi_core::tool::ToolResult::Output(mut s)) => {
                                    let mut last = String::new();
                                    while let Some(out) = s.next().await {
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
                                content: Content::text(result_str),
                            });
                        }
                    }

                    // Pass external (non-skill) calls upward or signal resume
                    if !external.is_empty() {
                        yield DeepAgentEvent::Agent(AgentEvent::NeedToolExecution {
                            state,
                            tool_calls: external,
                            completed_results: all_outcomes,
                        });
                        return;
                    }

                    // All skill calls handled — signal resume (empty tool_calls)
                    yield DeepAgentEvent::Agent(AgentEvent::NeedToolExecution {
                        state,
                        tool_calls: vec![],
                        completed_results: all_outcomes,
                    });
                    return;
                }
                other => yield other,
            }
        }
    }
}

fn make_tool_ctx(state: &remi_core::state::AgentState) -> ToolContext {
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

fn make_skill_event(tc: &ParsedToolCall, _ctx: &ToolContext) -> Option<SkillEvent> {
    match tc.name.as_str() {
        "skill__save" => {
            let name = tc.arguments["name"].as_str()?.to_string();
            // The actual path is returned in the tool result — we don't have it here
            // so we emit a placeholder.
            Some(SkillEvent::Saved {
                name: name.clone(),
                path: format!(".deepagent/skills/{name}.md"),
            })
        }
        "skill__delete" => {
            let name = tc.arguments["name"].as_str()?.to_string();
            Some(SkillEvent::Deleted { name })
        }
        _ => None,
    }
}

// ── SkillLayer ────────────────────────────────────────────────────────────────

pub struct SkillLayer<S> {
    pub store: S,
}

impl<A, S> remi_core::agent::Layer<A> for SkillLayer<S>
where
    A: Agent<Request = LoopInput, Response = DeepAgentEvent, Error = AgentError>,
    S: SkillStore,
{
    type Output = SkillAgent<A, S>;
    fn layer(self, inner: A) -> Self::Output {
        SkillAgent::new(inner, self.store)
    }
}
