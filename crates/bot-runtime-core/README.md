# bot-runtime-core

Reusable loop-driving primitives for markdown-defined agents.

This crate owns the low-level runtime boundary:

- parse markdown agent profiles
- apply profile defaults to `LoopInput`
- inject model-facing tool definitions
- drive model streams through `CoreAgentLoop`
- surface `CoreDriveEvent::ToolDispatch` when the model needs tools

It does not execute tools or emit product-level UI events. Callers such as `bot-core`
own approval, hooks, local/dynamic tool routing, `ToolCallResult` events, history,
state persistence, and channel-specific rendering.

```rust,no_run
use bot_runtime_core::{
    apply_profile_to_input, build_tool_definition_ctx, effective_agent_config,
    filter_tool_definitions, inject_extra_tools, AgentBuilder, AgentConfig, AgentProfile,
    CoreAgentLoop, CoreDriveConfig, CoreDriveEvent, DefaultToolRegistry, LoopInput,
    OpenAIClient, ToolRegistry,
};
use futures::StreamExt;

# async fn run() -> anyhow::Result<()> {
let markdown = r#"---
id: helper
name: Helper
model: gpt-4o
tools:
  - my_tool
---
You are a concise helper.
"#;

let profile = AgentProfile::from_markdown(markdown)?;
let tools = DefaultToolRegistry::new();
let config = effective_agent_config(&AgentConfig::from_env(), &profile);
let model = OpenAIClient::from_config(&config);
let inner = AgentBuilder::new().model(model).build();

let input = apply_profile_to_input(LoopInput::start("hello"), &profile, &config);
let tool_def_ctx = build_tool_definition_ctx(&input);
let tool_definitions =
    filter_tool_definitions(tools.definitions_with_context(&tool_def_ctx), &profile.tools);
let run_input = inject_extra_tools(input, tool_definitions);
let inner_events = inner.chat(run_input).await?;

let mut loop_driver = CoreAgentLoop::new(CoreDriveConfig::default());
let mut events = Box::pin(loop_driver.drive(inner_events));
while let Some(event) = events.next().await {
    match event {
        CoreDriveEvent::Text(delta) => print!("{delta}"),
        CoreDriveEvent::ToolDispatch(dispatch) => {
            // Execute tools in the caller, then resume the model with ToolCallOutcome values.
            let _tool_calls = dispatch.tool_calls;
            break;
        }
        CoreDriveEvent::Done { state, .. } => {
            let _latest_state = state;
            break;
        }
        CoreDriveEvent::Error { error, .. } => return Err(error.into()),
        _ => {}
    }
}
# Ok(())
# }
```

For externally executed tools, register existing model-facing definitions with
`register_dynamic_tool_definitions` or wrap a single definition with `DynamicTool`.
