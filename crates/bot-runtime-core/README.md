# bot-runtime-core

Reusable runtime primitives for running markdown-defined agents with injected tools.

```rust,no_run
use bot_runtime_core::{
    AgentBuilder, AgentConfig, CoreAgent, CoreAgentEvent, CoreStreamOptions, DefaultToolRegistry,
    OpenAIClient,
};
use futures::StreamExt;
use std::sync::Arc;

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

let tools = DefaultToolRegistry::new();
let core = CoreAgent::from_markdown(AgentConfig::from_env(), markdown, tools)?;
let model = OpenAIClient::from_config(&core.effective_agent_config());
let inner = AgentBuilder::new().model(model).build();
let cancel = Arc::new(tokio::sync::Notify::new());
let options = CoreStreamOptions::new().with_cancel(Arc::clone(&cancel));
let mut events = Box::pin(core.stream_text_with_options(inner, "hello", Vec::new(), options));

while let Some(event) = events.next().await {
    match event {
        CoreAgentEvent::Text(delta) => print!("{delta}"),
        CoreAgentEvent::Cancelled => break,
        CoreAgentEvent::Done { state } => {
            let _latest_state = state;
            break;
        }
        CoreAgentEvent::Error(error) => return Err(error.into()),
        _ => {}
    }
}
# Ok(())
# }
```

Register any `bot_runtime_core::Tool` in `DefaultToolRegistry` before constructing `CoreAgent`.
When a markdown profile lists `tools`, only those tool names are exposed and executable. If the
list is empty, all registered tools are available. `CoreAgent` owns the parsed markdown profile and
tool registry, so keep it around and call `stream_text`, `stream_content`, or `stream_loop_input`
for each new turn. Use the `_with_options` variants with `CoreStreamOptions` when the caller needs
to pass a cooperative cancel signal. `CoreAgentEvent::Done` carries the latest checkpoint state
when the underlying agent emitted one, so callers can persist it for resume.

For externally executed tools, register existing model-facing definitions with
`register_dynamic_tool_definitions` or wrap a single definition with `DynamicTool`.
