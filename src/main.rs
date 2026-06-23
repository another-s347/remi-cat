mod app;
mod channel;
mod cli;
mod command;
mod config;
mod core;
mod host_admin;
mod instance_profile;
mod local_trigger_scheduler;
mod profile_command;
mod runtime_config;
mod secret_store;
mod session;
mod tui_app;
mod tui_markdown;
mod tui_text;
mod web_chat;
mod workspace_files;

#[cfg(test)]
pub(crate) use app::{
    direct_workflow_options, CLI_CHANNEL, CLI_USERNAME, MAX_COMMAND_PREPROCESS_DEPTH,
};
pub(crate) use app::{
    CLI_CHAT_ID, SESSION_INPUT_HISTORY_METADATA_KEY, SESSION_MODEL_PROFILE_METADATA_KEY,
};
pub(crate) use cli::CliConfig;
pub(crate) use command::{process_runtime_commands, RuntimeCommandPipelineResult};
pub(crate) use core::{
    append_direct_sub_session_turn, sub_session_input_target, Runtime, SubSessionInputTarget,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::run().await
}
