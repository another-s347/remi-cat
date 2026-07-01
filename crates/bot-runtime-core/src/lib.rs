pub mod agent;
pub mod dynamic_tool;
pub mod events;
mod loop_driver;
pub mod profile;

pub use agent::{
    apply_profile_to_input, build_tool_definition_ctx, effective_agent_config, inject_extra_tools,
    tool_ctx_from_state, CoreAgent, CoreAgentConfig, CoreStreamOptions,
};
pub use dynamic_tool::{register_dynamic_tool_definitions, DynamicTool};
pub use events::CoreAgentEvent;
pub use loop_driver::{
    CoreAgentLoop, CoreCancelKind, CoreDriveConfig, CoreDriveEvent, CoreToolDispatch,
    CoreUsageStats,
};
pub use profile::{
    embedded_agent_profile, install_embedded_agent_profiles, AgentModelBindings, AgentProfile,
    AgentRegistry,
};

pub use remi_agentloop::prelude::{
    Agent, AgentBuilder, AgentConfig, AgentError, AgentState, BuiltAgent, Content, ContentPart,
    LoopInput, Message, OpenAIClient, ParsedToolCall, ReqwestTransport, ResumePayload, StepConfig,
    Tool, ToolCallOutcome, ToolContext, ToolDefinition, ToolOutput, ToolResult,
};
pub use remi_agentloop::tool::registry::{DefaultToolRegistry, ToolRegistry};
pub use remi_agentloop::types::{AgentEvent, SubSessionEvent};
