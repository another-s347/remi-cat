//! `SubAgentTaskTool` — delegates tasks to a spawned inner agent.
//!
//! The tool accepts a `task` string, creates a fresh `AgentLoop` with bash
//! and filesystem tools, runs the task to completion, and returns the agent's
//! final text response.  This lets the outer agent offload focused subtasks
//! to a "worker" without polluting its own context.
//!
//! ## Claude Code inspiration
//!
//! Claude Code's "subagent" pattern: the orchestrator agent plans and breaks
//! down work, then delegates self-contained subtasks to isolated agents that
//! each get a clean context.  This prevents context bloat from large
//! intermediate outputs and allows independent retry of failed subtasks.

use async_stream::stream;
use futures::{Future, Stream, StreamExt};
use remi_core::agent::Agent;
use remi_core::builder::AgentBuilder;
use remi_core::error::AgentError;
use remi_core::model::ChatModel;
use remi_core::tool::{Tool, ToolContext, ToolOutput, ToolResult};
use remi_core::types::{
    AgentEvent, LoopInput, ResumePayload, SubSessionEvent, SubSessionEventPayload,
};
use remi_tool::{
    BashTool, LocalFsCreateTool, LocalFsLsTool, LocalFsReadTool, LocalFsRemoveTool,
    LocalFsWriteTool,
};
use serde_json::json;
use std::pin::Pin;
use std::sync::Arc;

// ── Type alias ────────────────────────────────────────────────────────────────

type AgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent>>>;

/// Object-safe runner type: takes an owned task string, returns a boxed stream future.
pub type RunnerFn = dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<AgentEventStream, AgentError>>>>
    + Send
    + Sync;

// ── SubAgentTaskTool ──────────────────────────────────────────────────────────

/// A tool that delegates a task to a sub-agent and returns its final response.
pub struct SubAgentTaskTool {
    runner: Arc<RunnerFn>,
    tool_description: String,
    agent_name: String,
}

impl SubAgentTaskTool {
    /// Build a `SubAgentTaskTool` backed by `model`.
    ///
    /// Each invocation constructs a temporary `AgentLoop<M>` with bash + fs
    /// tools, cloning `model` so the original remains usable.
    pub fn new<M>(model: M, system_prompt: impl Into<String>, max_turns: usize) -> Self
    where
        M: ChatModel + Clone + Send + Sync + 'static,
    {
        let system_prompt = system_prompt.into();
        let runner: Arc<RunnerFn> = Arc::new(move |task: String| {
            let model = model.clone();
            let system_prompt = system_prompt.clone();
            Box::pin(async move {
                let agent = AgentBuilder::new()
                    .model(model)
                    .system(system_prompt)
                    .tool(BashTool)
                    .tool(LocalFsReadTool)
                    .tool(LocalFsWriteTool)
                    .tool(LocalFsCreateTool)
                    .tool(LocalFsRemoveTool)
                    .tool(LocalFsLsTool)
                    .max_turns(max_turns)
                    .build_loop();

                Ok(Box::pin(stream! {
                    match agent.chat(LoopInput::start(&task)).await {
                        Ok(inner_stream) => {
                            let mut inner_stream = std::pin::pin!(inner_stream);
                            while let Some(event) = inner_stream.next().await {
                                yield event;
                            }
                        }
                        Err(error) => {
                            yield AgentEvent::Error(error);
                        }
                    }
                }) as AgentEventStream)
            })
        });

        Self {
            runner,
            tool_description: "Delegate a focused subtask to a worker sub-agent. \
                The sub-agent has access to bash and filesystem tools. \
                Use this for self-contained tasks (file operations, code generation, \
                research) that you want to keep isolated from the main context."
                .to_string(),
            agent_name: "worker".to_string(),
        }
    }
}

// ── Tool impl ─────────────────────────────────────────────────────────────────

impl Tool for SubAgentTaskTool {
    fn name(&self) -> &str {
        "task__run"
    }

    fn description(&self) -> &str {
        &self.tool_description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Complete, self-contained task description for the sub-agent. \
                        Include all necessary context since the sub-agent starts with a clean history."
                }
            },
            "required": ["task"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let task = arguments["task"]
            .as_str()
            .ok_or_else(|| AgentError::tool("task__run", "missing 'task'"))?
            .to_string();

        let runner = Arc::clone(&self.runner);
        let agent_name = self.agent_name.clone();
        let title = Some(task.clone());

        Ok(ToolResult::Output(stream! {
            let inner_stream = match (runner)(task).await {
                Ok(stream) => stream,
                Err(error) => {
                    yield ToolOutput::Delta(format!("[sub-agent failed to start: {error}]"));
                    yield ToolOutput::text(format!("Sub-agent failed to start: {error}"));
                    return;
                }
            };

            let mut inner_stream = std::pin::pin!(inner_stream);
            let mut sub_thread_id = None;
            let mut sub_run_id = None;
            let mut final_output = String::new();

            while let Some(event) = inner_stream.next().await {
                match event {
                    AgentEvent::RunStart { thread_id, run_id, .. } => {
                        sub_thread_id = Some(thread_id.clone());
                        sub_run_id = Some(run_id.clone());
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            String::new(),
                            thread_id,
                            run_id,
                            agent_name.clone(),
                            title.clone(),
                            0,
                            SubSessionEventPayload::Start,
                        ));
                    }
                    AgentEvent::TextDelta(content) => {
                        final_output.push_str(&content);
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::Delta { content },
                            ));
                        }
                    }
                    AgentEvent::ThinkingStart => {
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::ThinkingStart,
                            ));
                        }
                    }
                    AgentEvent::ThinkingEnd { content } => {
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::ThinkingEnd { content },
                            ));
                        }
                    }
                    AgentEvent::ToolCallStart { id, name } => {
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::ToolCallStart { id, name },
                            ));
                        }
                    }
                    AgentEvent::ToolCallArgumentsDelta { id, delta } => {
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::ToolCallArgumentsDelta { id, delta },
                            ));
                        }
                    }
                    AgentEvent::ToolDelta { id, name, delta } => {
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::ToolDelta { id, name, delta },
                            ));
                        }
                    }
                    AgentEvent::ToolResult { id, name, result } => {
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::ToolResult { id, name, result },
                            ));
                        }
                    }
                    AgentEvent::TurnStart { turn } => {
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::TurnStart { turn },
                            ));
                        }
                    }
                    AgentEvent::Done => {
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::Done {
                                    final_output: if final_output.trim().is_empty() {
                                        None
                                    } else {
                                        Some(final_output.clone())
                                    },
                                },
                            ));
                        }
                        yield ToolOutput::text(final_output.clone());
                        return;
                    }
                    AgentEvent::Error(error) => {
                        let message = error.to_string();
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::Error {
                                    message: message.clone(),
                                },
                            ));
                        }
                        yield ToolOutput::text(format!("Sub-agent failed: {message}"));
                        return;
                    }
                    AgentEvent::Interrupt { interrupts } => {
                        let message = format!("Sub-agent interrupted with {} pending action(s)", interrupts.len());
                        if let (Some(thread_id), Some(run_id)) = (&sub_thread_id, &sub_run_id) {
                            yield ToolOutput::SubSession(SubSessionEvent::new(
                                String::new(),
                                thread_id.clone(),
                                run_id.clone(),
                                agent_name.clone(),
                                title.clone(),
                                0,
                                SubSessionEventPayload::Error {
                                    message: message.clone(),
                                },
                            ));
                        }
                        yield ToolOutput::text(message);
                        return;
                    }
                    AgentEvent::Cancelled | AgentEvent::Checkpoint(_) | AgentEvent::NeedToolExecution { .. } | AgentEvent::Usage { .. } | AgentEvent::SubSession(_) => {}
                }
            }

            yield ToolOutput::text(final_output);
        }))
    }
}
