use bot_runtime_core::{
    apply_profile_to_input, build_tool_definition_ctx, effective_agent_config,
    filter_tool_definitions, inject_extra_tools, Agent, AgentConfig, AgentError, AgentEvent,
    AgentProfile, AgentState, Content, CoreAgentLoop, CoreDriveConfig, CoreDriveEvent,
    DefaultToolRegistry, LoopInput, ParsedToolCall, StepConfig, Tool, ToolContext, ToolOutput,
    ToolRegistry, ToolResult,
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
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
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
        let LoopInput::Start {
            history,
            extra_tools,
            model,
            ..
        } = req
        else {
            panic!("expected start input");
        };
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
}

#[tokio::test]
async fn markdown_profile_feeds_core_loop_tool_dispatch() {
    let markdown = r#"---
id: integration
name: Integration Agent
model: integration-model
tools:
  - public_echo
---
Public markdown prompt.
"#;
    let profile = AgentProfile::from_markdown(markdown).unwrap();
    let mut tools = DefaultToolRegistry::new();
    tools.register(PublicEchoTool);
    let effective_config = effective_agent_config(&AgentConfig::default(), &profile);
    let input = apply_profile_to_input(LoopInput::start("hello"), &profile, &effective_config);
    let tool_def_ctx = build_tool_definition_ctx(&input);
    let tool_definitions = filter_tool_definitions(
        tools.definitions_with_context(&tool_def_ctx),
        &profile.tools,
    );
    let run_input = inject_extra_tools(input, tool_definitions);
    let inner_events = PublicInnerAgent.chat(run_input).await.unwrap();
    let mut agent_loop = CoreAgentLoop::new(CoreDriveConfig::default());
    let events = agent_loop.drive(inner_events).collect::<Vec<_>>().await;

    assert!(matches!(
        events.as_slice(),
        [CoreDriveEvent::ToolDispatch(dispatch)]
            if dispatch.tool_calls.len() == 1
                && dispatch.tool_calls[0].id == "call-public-echo"
                && dispatch.tool_calls[0].name == "public_echo"
    ));
}
