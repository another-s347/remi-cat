use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::stream;
use futures::{Stream, StreamExt};
use remi_agentloop_core::agent::Agent;
use remi_agentloop_core::builder::AgentBuilder;
use remi_agentloop_core::error::AgentError;
use remi_agentloop_core::tool::{
    FunctionDefinition, Tool, ToolContext, ToolDefinition, ToolDefinitionContext, ToolOutput,
    ToolResult,
};
use remi_agentloop_core::types::{AgentEvent, ChatRequest, ChatResponseChunk, LoopInput};
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
        Ok(stream::iter(response))
    }
}

struct MetadataHintTool;

impl Tool for MetadataHintTool {
    fn name(&self) -> &str {
        "metadata_hint"
    }

    fn description(&self) -> &str {
        "A tool used to verify metadata-driven extra prompts."
    }

    fn extra_prompt(&self, ctx: &ToolDefinitionContext) -> Option<String> {
        ctx.metadata
            .as_ref()
            .and_then(|metadata| metadata.get("tool_hint"))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _resume: Option<remi_agentloop_core::types::ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
            "ok",
        )])))
    }
}

struct StatefulPromptTool;

impl Tool for StatefulPromptTool {
    fn name(&self) -> &str {
        "stateful_prompt"
    }

    fn description(&self) -> &str {
        "Mutates user_state so the next request advertises a different extra prompt."
    }

    fn extra_prompt(&self, ctx: &ToolDefinitionContext) -> Option<String> {
        let status = ctx
            .user_state
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("cold");
        Some(format!("status={status}"))
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _resume: Option<remi_agentloop_core::types::ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let mut user_state = ctx.user_state.write().unwrap();
        user_state["status"] = json!("warm");
        Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
            "updated",
        )])))
    }
}

async fn drain_events(stream: &mut (impl Stream<Item = AgentEvent> + Unpin)) {
    while stream.next().await.is_some() {}
}

#[tokio::test]
async fn metadata_extra_prompt_is_in_first_request() {
    let model = RecordingModel::new(vec![vec![ChatResponseChunk::Done]]);
    let agent = AgentBuilder::new()
        .model(model.clone())
        .tool(MetadataHintTool)
        .build_loop();

    let mut stream = agent
        .chat(LoopInput::start("hello").metadata(json!({ "tool_hint": "prefer yaml" })))
        .await
        .unwrap();
    drain_events(&mut stream).await;

    let requests = model.requests();
    let tools = requests[0].tools.as_ref().unwrap();
    assert_eq!(
        tools[0].function.extra_prompt.as_deref(),
        Some("prefer yaml")
    );
}

#[tokio::test]
async fn user_state_change_refreshes_next_request_extra_prompt() {
    let model = RecordingModel::new(vec![
        vec![
            ChatResponseChunk::ToolCallStart {
                index: 0,
                id: "call-1".into(),
                name: "stateful_prompt".into(),
            },
            ChatResponseChunk::ToolCallDelta {
                index: 0,
                arguments_delta: "{}".into(),
            },
            ChatResponseChunk::Done,
        ],
        vec![ChatResponseChunk::Done],
    ]);

    let agent = AgentBuilder::new()
        .model(model.clone())
        .tool(StatefulPromptTool)
        .max_turns(4)
        .build_loop();

    let mut stream = agent.chat(LoopInput::start("hello")).await.unwrap();
    drain_events(&mut stream).await;

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].tools.as_ref().unwrap()[0]
            .function
            .extra_prompt
            .as_deref(),
        Some("status=cold")
    );
    assert_eq!(
        requests[1].tools.as_ref().unwrap()[0]
            .function
            .extra_prompt
            .as_deref(),
        Some("status=warm")
    );
}

#[tokio::test]
async fn extra_tools_preserve_extra_prompt() {
    let model = RecordingModel::new(vec![vec![ChatResponseChunk::Done]]);
    let agent = AgentBuilder::new().model(model.clone()).build_loop();

    let extra_tool = ToolDefinition {
        tool_type: "function".into(),
        function: FunctionDefinition {
            name: "external_tool".into(),
            description: "Externally injected tool".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            extra_prompt: Some("external runtime note".into()),
        },
    };

    let mut stream = agent
        .chat(LoopInput::start("hello").extra_tools(vec![extra_tool]))
        .await
        .unwrap();
    drain_events(&mut stream).await;

    let requests = model.requests();
    let tools = requests[0].tools.as_ref().unwrap();
    assert_eq!(
        tools[0].function.extra_prompt.as_deref(),
        Some("external runtime note")
    );
}
