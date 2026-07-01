use bot_runtime_core::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentState, Content, CoreAgent, CoreAgentEvent,
    DefaultToolRegistry, LoopInput, ParsedToolCall, StepConfig, Tool, ToolCallOutcome, ToolContext,
    ToolOutput, ToolResult,
};
use futures::{stream, Stream, StreamExt};

struct PublicEchoTool;

impl Tool for PublicEchoTool {
    fn name(&self) -> &str {
        "public_echo"
    }

    fn description(&self) -> &str {
        "Echoes from an integration-test tool."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _resume: Option<bot_runtime_core::ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        Ok(ToolResult::Output(stream::iter(vec![ToolOutput::text(
            "public-tool-output",
        )])))
    }
}

struct PublicInnerAgent;

impl Agent for PublicInnerAgent {
    type Request = LoopInput;
    type Response = AgentEvent;
    type Error = AgentError;

    async fn chat(&self, req: Self::Request) -> Result<impl Stream<Item = AgentEvent>, AgentError> {
        match req {
            LoopInput::Start {
                history,
                extra_tools,
                model,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("integration-model"));
                assert!(history.iter().any(|message| {
                    matches!(&message.content, Content::Text(text) if text == "Public markdown prompt.")
                }));
                assert_eq!(
                    extra_tools
                        .iter()
                        .map(|definition| definition.function.name.as_str())
                        .collect::<Vec<_>>(),
                    vec!["public_echo"]
                );
                Ok(stream::iter(vec![AgentEvent::NeedToolExecution {
                    state: AgentState::new(StepConfig::new("integration-model")),
                    tool_calls: vec![ParsedToolCall {
                        id: "call-public-echo".to_string(),
                        name: "public_echo".to_string(),
                        arguments: serde_json::json!({}),
                    }],
                    completed_results: Vec::new(),
                }]))
            }
            LoopInput::Resume { results, .. } => {
                let text = results
                    .into_iter()
                    .find_map(|result| match result {
                        ToolCallOutcome::Result { content, .. } => Some(content.text_content()),
                        ToolCallOutcome::Error { .. } => None,
                    })
                    .unwrap_or_default();
                Ok(stream::iter(vec![
                    AgentEvent::TextDelta(text),
                    AgentEvent::Done,
                ]))
            }
            LoopInput::Cancel { .. } => Ok(stream::iter(vec![AgentEvent::Cancelled])),
        }
    }
}

#[tokio::test]
async fn external_style_markdown_agent_with_injected_tool_runs() {
    let markdown = r#"---
id: integration
name: Integration Agent
model: integration-model
tools:
  - public_echo
---
Public markdown prompt.
"#;
    let mut tools = DefaultToolRegistry::new();
    tools.register(PublicEchoTool);
    let core = CoreAgent::from_markdown(AgentConfig::default(), markdown, tools).unwrap();

    let events = core
        .stream_text(PublicInnerAgent, "hello", Vec::new())
        .collect::<Vec<_>>()
        .await;

    assert!(events
        .iter()
        .any(|event| matches!(event, CoreAgentEvent::Text(text) if text == "public-tool-output")));
    assert!(events
        .iter()
        .any(|event| matches!(event, CoreAgentEvent::Done { .. })));
}
