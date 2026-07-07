use async_stream::stream;
use futures::{Future, Stream, StreamExt};
use remi_core::prelude::{
    AgentError, AgentEvent, ResumePayload, SubSessionEvent, SubSessionEventPayload, Tool,
    ToolContext, ToolOutput, ToolResult,
};
use serde_json::Value as JsonValue;
use std::pin::Pin;
use std::sync::Arc;

pub type SubAgentEventStream = Pin<Box<dyn Stream<Item = AgentEvent>>>;

type TitleFn = dyn Fn(&JsonValue) -> Option<String> + Send + Sync;
type RunnerFn = dyn Fn(
        JsonValue,
        ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<SubAgentEventStream, AgentError>>>>
    + Send
    + Sync;

/// Adapts an inner agent run into a normal tool call with structured sub-session output.
///
/// The inner agent's intermediate events are surfaced as `ToolOutput::SubSession(...)`, while
/// only the final answer is returned to the parent loop as the tool result.
pub struct SubAgentToolAdapter {
    name: String,
    description: String,
    parameters_schema: JsonValue,
    agent_name: String,
    title_from_args: Arc<TitleFn>,
    runner: Arc<RunnerFn>,
}

impl SubAgentToolAdapter {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters_schema: JsonValue,
        agent_name: impl Into<String>,
        title_from_args: impl Fn(&JsonValue) -> Option<String> + Send + Sync + 'static,
        runner: impl Fn(
                JsonValue,
                ToolContext,
            )
                -> Pin<Box<dyn Future<Output = Result<SubAgentEventStream, AgentError>>>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters_schema,
            agent_name: agent_name.into(),
            title_from_args: Arc::new(title_from_args),
            runner: Arc::new(runner),
        }
    }
}

impl Tool for SubAgentToolAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> JsonValue {
        self.parameters_schema.clone()
    }

    async fn execute(
        &self,
        arguments: JsonValue,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let runner = Arc::clone(&self.runner);
        let agent_name = self.agent_name.clone();
        let title = (self.title_from_args)(&arguments);
        let tool_ctx = ctx.clone();

        Ok(ToolResult::Output(stream! {
            let inner_stream = match (runner)(arguments, tool_ctx).await {
                Ok(stream) => stream,
                Err(error) => {
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
                        let message =
                            format!("Sub-agent interrupted with {} pending action(s)", interrupts.len());
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
                    AgentEvent::Cancelled
                    | AgentEvent::Checkpoint(_)
                    | AgentEvent::NeedToolExecution { .. }
                    | AgentEvent::Usage { .. }
                    | AgentEvent::SubSession(_) => {}
                }
            }

            yield ToolOutput::text(final_output);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;
    use remi_core::prelude::{AgentConfig, RunId, ThreadId};
    use serde_json::json;
    use std::sync::{Arc, RwLock};

    #[tokio::test]
    async fn adapter_streams_sub_session_but_returns_only_final_result() {
        let tool = SubAgentToolAdapter::new(
            "specialist",
            "Test specialist.",
            json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            "tester",
            |arguments| {
                arguments
                    .get("query")
                    .and_then(JsonValue::as_str)
                    .map(ToString::to_string)
            },
            |_arguments, _ctx| {
                Box::pin(async move {
                    Ok(Box::pin(stream::iter(vec![
                        AgentEvent::RunStart {
                            thread_id: ThreadId("sub-thread".into()),
                            run_id: RunId("sub-run".into()),
                            metadata: None,
                        },
                        AgentEvent::ToolCallStart {
                            id: "tool-1".into(),
                            name: "calc".into(),
                        },
                        AgentEvent::TextDelta("42".into()),
                        AgentEvent::Done,
                    ])) as SubAgentEventStream)
                })
            },
        );

        let ctx = ToolContext {
            config: AgentConfig::default(),
            thread_id: Some(ThreadId("parent-thread".into())),
            run_id: RunId("parent-run".into()),
            metadata: None,
            cancel: None,
            user_state: Arc::new(RwLock::new(JsonValue::Null)),
        };

        let result = tool
            .execute(json!({ "query": "what is 6*7" }), None, &ctx)
            .await
            .expect("tool executes");

        let ToolResult::Output(stream) = result else {
            panic!("expected output stream");
        };

        let outputs = stream.collect::<Vec<_>>().await;

        assert!(outputs.iter().any(|item| matches!(
            item,
            ToolOutput::SubSession(SubSessionEvent {
                payload: SubSessionEventPayload::Start,
                ..
            })
        )));
        assert!(outputs.iter().any(|item| matches!(
            item,
            ToolOutput::SubSession(SubSessionEvent {
                payload: SubSessionEventPayload::ToolCallStart { name, .. },
                ..
            }) if name == "calc"
        )));
        assert!(outputs.iter().any(|item| matches!(
            item,
            ToolOutput::Result(content) if content.text_content() == "42"
        )));
    }
}
