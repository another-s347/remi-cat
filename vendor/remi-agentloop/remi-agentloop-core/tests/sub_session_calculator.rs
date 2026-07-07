use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_agentloop_core::agent::Agent;
use remi_agentloop_core::builder::AgentBuilder;
use remi_agentloop_core::error::AgentError;
use remi_agentloop_core::tool::{Tool, ToolContext, ToolOutput, ToolResult};
use remi_agentloop_core::types::{
    AgentEvent, ChatRequest, ChatResponseChunk, LoopInput, ResumePayload, Role, SubSessionEvent,
    SubSessionEventPayload,
};
use serde_json::json;

#[derive(Clone)]
struct RecordingModel {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
    responses: Arc<Mutex<VecDeque<Vec<ChatResponseChunk>>>>,
}

impl RecordingModel {
    fn new(responses: Vec<Vec<ChatResponseChunk>>) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
        }
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Agent for RecordingModel {
    type Request = ChatRequest;
    type Response = ChatResponseChunk;
    type Error = AgentError;

    async fn chat(
        &self,
        req: Self::Request,
    ) -> Result<impl Stream<Item = Self::Response>, Self::Error> {
        self.requests.lock().unwrap().push(req);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![ChatResponseChunk::Done]);
        Ok(futures::stream::iter(response))
    }
}

struct AddTool;

impl Tool for AddTool {
    fn name(&self) -> &str {
        "add"
    }

    fn description(&self) -> &str {
        "Add two numbers."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" }
            },
            "required": ["a", "b"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let a = arguments
            .get("a")
            .and_then(|value| value.as_f64())
            .unwrap_or_default();
        let b = arguments
            .get("b")
            .and_then(|value| value.as_f64())
            .unwrap_or_default();
        Ok(ToolResult::Output(futures::stream::iter(vec![
            ToolOutput::text((a + b).to_string()),
        ])))
    }
}

struct MultiplyTool;

impl Tool for MultiplyTool {
    fn name(&self) -> &str {
        "multiply"
    }

    fn description(&self) -> &str {
        "Multiply two numbers."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "a": { "type": "number" },
                "b": { "type": "number" }
            },
            "required": ["a", "b"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let a = arguments
            .get("a")
            .and_then(|value| value.as_f64())
            .unwrap_or_default();
        let b = arguments
            .get("b")
            .and_then(|value| value.as_f64())
            .unwrap_or_default();
        Ok(ToolResult::Output(futures::stream::iter(vec![
            ToolOutput::text((a * b).to_string()),
        ])))
    }
}

struct CalculatorSubAgentTool {
    inner_model: RecordingModel,
}

impl CalculatorSubAgentTool {
    fn emit_sub_event(
        payload: SubSessionEventPayload,
        thread_id: &Option<remi_agentloop_core::types::ThreadId>,
        run_id: &Option<remi_agentloop_core::types::RunId>,
    ) -> Option<ToolOutput> {
        Some(ToolOutput::SubSession(SubSessionEvent::new(
            String::new(),
            thread_id.clone()?,
            run_id.clone()?,
            "calculator",
            Some("calculator".to_string()),
            0,
            payload,
        )))
    }
}

impl Tool for CalculatorSubAgentTool {
    fn name(&self) -> &str {
        "calculator_subagent"
    }

    fn description(&self) -> &str {
        "Delegate arithmetic to an isolated calculator sub-agent."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "expression": { "type": "string" }
            },
            "required": ["expression"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let expression = arguments
            .get("expression")
            .and_then(|value| value.as_str())
            .ok_or_else(|| AgentError::tool("calculator_subagent", "missing expression"))?
            .to_string();
        let inner_model = self.inner_model.clone();

        Ok(ToolResult::Output(stream! {
            let agent = AgentBuilder::new()
                .model(inner_model)
                .system("You are a precise calculator. Always use tools.")
                .tool(AddTool)
                .tool(MultiplyTool)
                .max_turns(6)
                .build_loop();

            let inner_stream = match agent.chat(LoopInput::start(expression)).await {
                Ok(stream) => stream,
                Err(error) => {
                    yield ToolOutput::text(format!("Sub-agent failed: {error}"));
                    return;
                }
            };

            let mut inner_stream = std::pin::pin!(inner_stream);
            let mut thread_id = None;
            let mut run_id = None;
            let mut final_output = String::new();

            while let Some(event) = inner_stream.next().await {
                match event {
                    AgentEvent::RunStart { thread_id: sub_thread_id, run_id: sub_run_id, .. } => {
                        thread_id = Some(sub_thread_id.clone());
                        run_id = Some(sub_run_id.clone());
                        if let Some(output) = Self::emit_sub_event(SubSessionEventPayload::Start, &thread_id, &run_id) {
                            yield output;
                        }
                    }
                    AgentEvent::TextDelta(content) => {
                        final_output.push_str(&content);
                        if let Some(output) = Self::emit_sub_event(SubSessionEventPayload::Delta { content }, &thread_id, &run_id) {
                            yield output;
                        }
                    }
                    AgentEvent::ToolCallStart { id, name } => {
                        if let Some(output) = Self::emit_sub_event(SubSessionEventPayload::ToolCallStart { id, name }, &thread_id, &run_id) {
                            yield output;
                        }
                    }
                    AgentEvent::ToolCallArgumentsDelta { id, delta } => {
                        if let Some(output) = Self::emit_sub_event(SubSessionEventPayload::ToolCallArgumentsDelta { id, delta }, &thread_id, &run_id) {
                            yield output;
                        }
                    }
                    AgentEvent::ToolResult { id, name, result } => {
                        if let Some(output) = Self::emit_sub_event(SubSessionEventPayload::ToolResult { id, name, result }, &thread_id, &run_id) {
                            yield output;
                        }
                    }
                    AgentEvent::Done => {
                        if let Some(output) = Self::emit_sub_event(SubSessionEventPayload::Done { final_output: Some(final_output.clone()) }, &thread_id, &run_id) {
                            yield output;
                        }
                        yield ToolOutput::text(final_output.clone());
                        return;
                    }
                    AgentEvent::Error(error) => {
                        if let Some(output) = Self::emit_sub_event(SubSessionEventPayload::Error { message: error.to_string() }, &thread_id, &run_id) {
                            yield output;
                        }
                        yield ToolOutput::text(format!("Sub-agent failed: {error}"));
                        return;
                    }
                    AgentEvent::ThinkingStart
                    | AgentEvent::ThinkingEnd { .. }
                    | AgentEvent::ToolDelta { .. }
                    | AgentEvent::Interrupt { .. }
                    | AgentEvent::TurnStart { .. }
                    | AgentEvent::Usage { .. }
                    | AgentEvent::Cancelled
                    | AgentEvent::Checkpoint(_)
                    | AgentEvent::NeedToolExecution { .. }
                    | AgentEvent::SubSession(_) => {}
                }
            }
        }))
    }
}

#[tokio::test]
async fn calculator_sub_agent_streams_sub_session_but_only_returns_final_tool_result() {
    let outer_model = RecordingModel::new(vec![
        vec![
            ChatResponseChunk::ToolCallStart {
                index: 0,
                id: "call-calc".into(),
                name: "calculator_subagent".into(),
            },
            ChatResponseChunk::ToolCallDelta {
                index: 0,
                arguments_delta: r#"{"expression":"(3 + 7) * 2"}"#.into(),
            },
            ChatResponseChunk::Done,
        ],
        vec![
            ChatResponseChunk::Delta {
                content: "20".into(),
                role: Some(Role::Assistant),
            },
            ChatResponseChunk::Done,
        ],
    ]);

    let inner_model = RecordingModel::new(vec![
        vec![
            ChatResponseChunk::ToolCallStart {
                index: 0,
                id: "call-add".into(),
                name: "add".into(),
            },
            ChatResponseChunk::ToolCallDelta {
                index: 0,
                arguments_delta: r#"{"a":3,"b":7}"#.into(),
            },
            ChatResponseChunk::Done,
        ],
        vec![
            ChatResponseChunk::ToolCallStart {
                index: 0,
                id: "call-multiply".into(),
                name: "multiply".into(),
            },
            ChatResponseChunk::ToolCallDelta {
                index: 0,
                arguments_delta: r#"{"a":10,"b":2}"#.into(),
            },
            ChatResponseChunk::Done,
        ],
        vec![
            ChatResponseChunk::Delta {
                content: "20".into(),
                role: Some(Role::Assistant),
            },
            ChatResponseChunk::Done,
        ],
    ]);

    let agent = AgentBuilder::new()
        .model(outer_model.clone())
        .tool(CalculatorSubAgentTool {
            inner_model: inner_model.clone(),
        })
        .max_turns(6)
        .build_loop();

    let stream = agent
        .chat(LoopInput::start("Calculate (3 + 7) * 2"))
        .await
        .unwrap();
    let events = stream.collect::<Vec<_>>().await;

    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SubSession(SubSessionEvent {
            payload: SubSessionEventPayload::Start,
            ..
        })
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SubSession(SubSessionEvent {
            payload: SubSessionEventPayload::ToolCallStart { name, .. },
            ..
        }) if name == "add"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::SubSession(SubSessionEvent {
            payload: SubSessionEventPayload::ToolCallStart { name, .. },
            ..
        }) if name == "multiply"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ToolResult { id, name, result }
        if id == "call-calc" && name == "calculator_subagent" && result == "20"
    )));

    let outer_requests = outer_model.requests();
    assert_eq!(outer_requests.len(), 2);

    let resumed_messages = &outer_requests[1].messages;
    assert_eq!(resumed_messages.len(), 3);
    assert!(resumed_messages
        .iter()
        .all(|message| !message.content.text_content().contains("call-add")));
    assert!(resumed_messages
        .iter()
        .all(|message| !message.content.text_content().contains("multiply")));
    assert_eq!(resumed_messages[2].content.text_content(), "20");
}
