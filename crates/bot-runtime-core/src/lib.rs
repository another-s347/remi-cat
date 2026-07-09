pub mod agent;
pub mod dynamic_tool;
mod loop_driver;
pub mod profile;

pub use agent::{
    apply_profile_to_input, build_tool_definition_ctx, chat_ctx_from_input, effective_agent_config,
    filter_tool_definitions, inject_extra_tools, tool_allowed, tool_ctx_from_state,
    tool_ctx_from_state_with_cancel, tool_definition_ctx_from_chat_ctx,
    tool_definition_ctx_from_state, CoreSteerBatch, CoreSteerInput, CoreSteerQueue,
    CoreSteerSource, CoreStreamOptions,
};
pub use dynamic_tool::{register_dynamic_tool_definitions, DynamicTool};
pub use loop_driver::{
    CoreAgentLoop, CoreCancelKind, CoreDriveConfig, CoreDriveEvent, CoreToolDispatch,
    CoreUsageStats,
};
pub use profile::{
    embedded_agent_profile, install_embedded_agent_profiles, AgentModelBindings, AgentProfile,
    AgentRegistry,
};

pub use remi_agentloop::prelude::ChatCtx as ToolContext;
pub use remi_agentloop::prelude::{
    Agent, AgentBuilder, AgentConfig, AgentError, AgentState, BuiltAgent, ChatCtx, ChatCtxState,
    Content, ContentPart, LoopInput, Message, OpenAIClient, ParsedToolCall, ReqwestTransport,
    ResumePayload, StepConfig, Tool, ToolCallOutcome, ToolDefinition, ToolOutput, ToolResult,
};
pub use remi_agentloop::tool::registry::{DefaultToolRegistry, ToolRegistry};
pub use remi_agentloop::types::{AgentEvent, SubSessionEvent};
