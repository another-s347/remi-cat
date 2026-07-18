mod acp_agent;
mod app;
mod application;
mod channel;
mod cli;
mod codex_acp_adapter;
mod command;
mod config;
mod core;
mod host_admin;
mod instance_profile;
mod model_input_store;
mod profile_command;
mod runtime_config;
mod secret_store;
mod session;
mod telemetry;
mod tui_app;
mod tui_form;
mod tui_markdown;
mod tui_text;
mod tui_theme;
mod web_chat;
mod workspace_files;

pub use application::{
    ActiveRunInfo, ActiveRunState, Application, ApplicationBuilder, ApplicationCatalog,
    ApplicationEvent, ApplicationHandle, ApplicationInfo, ChannelConfig, ChannelHandle,
    CommandPreprocessResult, EffectiveSessionConfig, HookMutationResult, HookReloadReport,
    RunControl, RunHandle, RunOptions, RunRequest, SessionPatch, SubSessionEventPage,
};
pub use bot_core::{
    AgentError, AgentProfile, BuiltinSkill, CatEvent, Content, DynamicTool, DynamicToolRisk,
    HookSource, HookStatus, HookTextFormat, ModelProfileConfig, ReasoningEffort, SteerSubmitResult,
    ThreadHistoryMessage, ToolOutput, ToolResult, WorkflowDefinition,
};
pub use session::{ChannelBinding, Session, SubSession, SubSessionKind};

pub async fn run_cli() -> anyhow::Result<()> {
    app::run().await
}

#[cfg(test)]
pub(crate) use app::{
    direct_workflow_options, CLI_CHANNEL, CLI_USERNAME, MAX_COMMAND_PREPROCESS_DEPTH,
};
pub(crate) use app::{
    CLI_CHAT_ID, SESSION_AGENT_ID_METADATA_KEY, SESSION_INPUT_HISTORY_METADATA_KEY,
    SESSION_MODEL_PROFILE_METADATA_KEY,
};
pub(crate) use cli::CliConfig;
pub(crate) use core::{ChatChannel, ChatRequest, CoreChatEvent, Runtime};
