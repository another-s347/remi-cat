use crate::app::{CLI_CHAT_ID, CLI_USERNAME};
use crate::instance_profile::{self, InstanceProfile, DIAGNOSTIC_PROFILE_NAME};
use crate::profile_command::{self, ProfileCommand};
use clap::{Args, Parser, Subcommand};

pub(crate) const CLI_USER_ID: &str = "local-user";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppCommand {
    Run(CliConfig),
    Setup(Vec<String>),
    Doctor,
    Tools(ToolsArgs),
    Hooks(HooksCommand),
    Secrets(SecretCommand),
    ConfigSet(Vec<String>),
    SandboxSet(Vec<String>),
    Profile(ProfileCommand),
    Feishu(FeishuCommand),
    Codex(CodexCommand),
    Update(UpdateCommand),
    Feedback(FeedbackCommand),
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlobalArgs {
    pub(crate) profile: Option<String>,
    pub(crate) command_args: Vec<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "remi-cat",
    version,
    about = "Single-process AI agent runtime for Feishu/Lark, Web Chat, and local CLI",
    long_about = "remi-cat runs IM ingress, Web Chat, session routing, agent execution, ACP support, and local tooling in one host process."
)]
pub(crate) struct CliArgs {
    #[arg(
        long,
        global = true,
        value_name = "NAME",
        value_parser = validate_profile_arg,
        help = "Select a named runtime profile"
    )]
    profile: Option<String>,

    #[arg(
        long = "tool-output-overflow-bytes",
        visible_alias = "overflow-bytes",
        global = true,
        value_name = "BYTES",
        value_parser = validate_positive_usize_arg,
        help = "Override the tool-output overflow threshold in bytes"
    )]
    pub(crate) tool_output_overflow_bytes: Option<usize>,

    #[command(subcommand)]
    command: Option<CliCommand>,

    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, Args, Default)]
struct RunArgs {
    #[arg(long = "local", help = "Start local CLI chat mode")]
    local: bool,

    #[arg(long = "cli-im", help = "Start local CLI chat mode")]
    cli_im: bool,

    #[arg(
        long = "cli-im-once",
        alias = "cli-message",
        short = 'm',
        num_args = 1..,
        trailing_var_arg = true,
        value_name = "MESSAGE",
        help = "Send one local CLI message and exit"
    )]
    cli_message: Vec<String>,

    #[arg(long = "admin-only", help = "Serve only the local management API")]
    admin_only: bool,

    #[arg(
        short = 'p',
        long = "prompt",
        num_args = 1..,
        trailing_var_arg = true,
        value_name = "PROMPT",
        help = "Send one prompt-style local message and exit"
    )]
    prompt: Vec<String>,

    #[arg(
        long = "resume",
        value_name = "SESSION_ID",
        num_args = 0..=1,
        require_equals = false,
        help = "Open the TUI resume picker or resume a specific TUI session"
    )]
    resume: Option<Option<String>>,

    #[arg(
        long = "cli-channel",
        visible_alias = "channel",
        visible_alias = "session",
        default_value = CLI_CHAT_ID,
        value_name = "ID",
        help = "CLI channel/session id"
    )]
    channel_id: String,

    #[arg(
        long = "cli-user",
        visible_alias = "user",
        default_value = CLI_USER_ID,
        value_name = "ID",
        help = "CLI user id"
    )]
    user_id: String,

    #[arg(
        long = "cli-name",
        visible_alias = "name",
        default_value = CLI_USERNAME,
        value_name = "NAME",
        help = "CLI display name"
    )]
    username: String,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    #[command(about = "Run the interactive setup wizard or non-interactive setup")]
    Setup(SetupArgs),
    #[command(
        alias = "check",
        about = "Inspect local runtime configuration and readiness"
    )]
    Doctor,
    #[command(about = "List all runtime-registered tools with configuration diagnostics")]
    Tools(ToolsArgs),
    #[command(about = "List, trust, enable, or disable Codex-compatible hooks")]
    Hooks(HooksArgs),
    #[command(alias = "secret", about = "List, read, set, or delete secrets")]
    Secrets(SecretsArgs),
    #[command(about = "Update runtime config")]
    Config(ConfigArgs),
    #[command(about = "Update sandbox runtime config")]
    Sandbox(SandboxArgs),
    #[command(about = "Manage runtime profiles")]
    Profile {
        #[command(subcommand)]
        command: ProfileCliCommand,
    },
    #[command(about = "Manage Feishu/Lark setup")]
    Feishu {
        #[command(subcommand)]
        command: FeishuCliCommand,
    },
    #[command(about = "Configure and inspect Codex ACP")]
    Codex {
        #[command(subcommand)]
        command: CodexCliCommand,
    },
    #[command(about = "Check for or install remi-cat updates")]
    Update {
        #[command(subcommand)]
        command: UpdateCliCommand,
    },
    #[command(about = "Create a GitHub feedback issue")]
    Feedback(FeedbackArgs),
    #[command(about = "Create a GitHub issue")]
    Issue {
        #[command(subcommand)]
        command: IssueCliCommand,
    },
    #[command(about = "Start local CLI chat mode")]
    Cli(LocalChatArgs),
    #[command(about = "Start terminal UI mode")]
    Tui(TuiArgs),
    #[command(about = "Send one prompt-style local message and exit")]
    Prompt(PromptArgs),
    #[command(about = "Serve only the local management API")]
    Admin,
}

#[derive(Debug, Args)]
struct SetupArgs {
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "KEY=VALUE",
        help = "Non-interactive runtime config entries"
    )]
    entries: Vec<String>,
}

#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub(crate) struct ToolsArgs {
    #[arg(long, help = "Print machine-readable JSON")]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
struct HooksArgs {
    #[command(subcommand)]
    command: Option<HooksCliCommand>,
}

#[derive(Debug, Subcommand)]
enum HooksCliCommand {
    #[command(about = "List configured hooks")]
    List {
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Trust one hook definition hash")]
    Trust { hash: String },
    #[command(about = "Enable one hook definition hash")]
    Enable { hash: String },
    #[command(about = "Disable one hook definition hash")]
    Disable { hash: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HooksCommand {
    List { json: bool },
    Trust { hash: String },
    Enable { hash: String },
    Disable { hash: String },
}

#[derive(Debug, Args)]
struct SecretsArgs {
    #[command(subcommand)]
    command: Option<SecretCliCommand>,
}

#[derive(Debug, Subcommand)]
enum SecretCliCommand {
    #[command(about = "List known secret keys")]
    List,
    #[command(about = "Print one secret value")]
    Get { key: String },
    #[command(about = "Set one secret value")]
    Set { key: String, value: String },
    #[command(alias = "remove", alias = "unset", about = "Delete one secret")]
    Delete { key: String },
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCliCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCliCommand {
    #[command(about = "Set runtime config entries")]
    Set(KeyValueArgs),
}

#[derive(Debug, Args)]
struct SandboxArgs {
    #[command(subcommand)]
    command: SandboxCliCommand,
}

#[derive(Debug, Subcommand)]
enum SandboxCliCommand {
    #[command(about = "Set sandbox config entries")]
    Set(KeyValueArgs),
}

#[derive(Debug, Args)]
struct KeyValueArgs {
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "KEY=VALUE"
    )]
    entries: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum FeishuCliCommand {
    #[command(
        about = "Initialize Feishu/Lark CLI and app credentials",
        after_help = "This configures credentials. Feishu/Lark chat routing is automatic from incoming IM events; local CLI sessions use `remi-cat cli --channel <id>`."
    )]
    Init,
    #[command(
        alias = "check",
        about = "Inspect Feishu/Lark CLI and credential readiness",
        after_help = "Checks lark-cli auth and remi-cat app credentials. It does not create or select a Feishu/Lark chat channel."
    )]
    Doctor,
}

#[derive(Debug, Subcommand)]
enum CodexCliCommand {
    #[command(
        about = "Configure local Codex as the ACP client",
        after_help = "Examples:\n  remi-cat codex setup\n  remi-cat codex setup --bin /usr/local/bin/codex --agent default\n\nThis writes `acp.mode=local`, `acp.client=codex`, and optional Codex binary/agent settings to the selected profile runtime config."
    )]
    Setup {
        #[arg(long = "bin", value_name = "PATH", help = "Path to the codex binary")]
        bin: Option<String>,
        #[arg(long, value_name = "NAME", help = "ACP agent name")]
        agent: Option<String>,
    },
    #[command(about = "Inspect Codex ACP configuration")]
    Doctor,
}

#[derive(Debug, Subcommand)]
enum UpdateCliCommand {
    #[command(
        about = "Check the latest GitHub release",
        after_help = "Queries GitHub releases for the configured remi-cat repository."
    )]
    Check {
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(
        name = "self",
        about = "Install a selected remi-cat release with cargo install",
        after_help = "Examples:\n  remi-cat update self --dry-run\n  remi-cat update self --version v0.2.1\n\nThe installer uses `cargo install --git https://github.com/another-s347/remi-cat.git --tag <version>`."
    )]
    SelfUpdate {
        #[arg(
            long,
            value_name = "VERSION_OR_TAG",
            help = "Release version or tag to install"
        )]
        version: Option<String>,
        #[arg(long, help = "Reinstall even when the selected version is not newer")]
        force: bool,
        #[arg(long, help = "Print the install command without running it")]
        dry_run: bool,
    },
}

#[derive(Debug, Args)]
struct FeedbackArgs {
    #[arg(short, long)]
    title: Option<String>,
    #[arg(short, long)]
    pub(crate) body: Option<String>,
    #[arg(long)]
    pub(crate) repo: Option<String>,
    #[arg(long = "label", visible_alias = "labels", value_delimiter = ',')]
    pub(crate) labels: Vec<String>,
    #[arg(long)]
    pub(crate) include_logs: bool,
    #[arg(long)]
    pub(crate) dry_run: bool,
    #[arg(long)]
    no_default_label: bool,
    #[arg(trailing_var_arg = true, value_name = "MESSAGE")]
    message: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum IssueCliCommand {
    #[command(about = "Create a GitHub issue using the feedback flow")]
    Create(FeedbackArgs),
}

#[derive(Debug, Args)]
struct LocalChatArgs {
    #[command(flatten)]
    common: LocalCommonArgs,
    #[arg(trailing_var_arg = true, value_name = "MESSAGE")]
    message: Vec<String>,
}

#[derive(Debug, Args)]
struct PromptArgs {
    #[command(flatten)]
    common: LocalCommonArgs,
    #[arg(required = true, trailing_var_arg = true, value_name = "PROMPT")]
    prompt: Vec<String>,
}

#[derive(Debug, Args)]
struct TuiArgs {
    #[command(flatten)]
    common: LocalCommonArgs,
    #[command(subcommand)]
    command: Option<TuiCliCommand>,
}

#[derive(Debug, Subcommand)]
enum TuiCliCommand {
    #[command(about = "Open the resume picker or resume a specific session")]
    Resume {
        #[arg(value_name = "SESSION_ID")]
        session_id: Option<String>,
        #[arg(
            long = "cli-channel",
            visible_alias = "channel",
            value_name = "ID",
            help = "Local CLI channel id; the same id resumes the same persisted session"
        )]
        channel_id: Option<String>,
        #[arg(
            long = "cli-user",
            visible_alias = "user",
            value_name = "ID",
            help = "Local CLI user id used for message metadata"
        )]
        user_id: Option<String>,
        #[arg(
            long = "cli-name",
            visible_alias = "name",
            value_name = "NAME",
            help = "Display name to include in local CLI message metadata"
        )]
        username: Option<String>,
    },
}

#[derive(Debug, Args)]
struct LocalCommonArgs {
    #[arg(
        long = "cli-channel",
        visible_alias = "channel",
        visible_alias = "session",
        default_value = CLI_CHAT_ID,
        value_name = "ID",
        help = "Local CLI channel id; the same id resumes the same persisted session"
    )]
    channel_id: String,
    #[arg(
        long = "cli-user",
        visible_alias = "user",
        default_value = CLI_USER_ID,
        value_name = "ID",
        help = "Local CLI user id used for message metadata"
    )]
    user_id: String,
    #[arg(
        long = "cli-name",
        visible_alias = "name",
        default_value = CLI_USERNAME,
        value_name = "NAME",
        help = "Local CLI display name used for message metadata"
    )]
    username: String,
}

#[derive(Debug, Subcommand)]
enum ProfileCliCommand {
    #[command(about = "List configured profiles")]
    List,
    #[command(about = "Show profile configuration and status")]
    Show { name: String },
    #[command(about = "Create a profile")]
    Create(ProfileCreateArgs),
    #[command(about = "Delete a profile")]
    Delete {
        name: String,
        #[arg(long)]
        force: bool,
    },
    #[command(about = "Start a profile in the background")]
    Start { name: String },
    #[command(about = "Stop a background profile process")]
    Stop {
        name: String,
        #[arg(long)]
        force: bool,
    },
    #[command(about = "Restart a background profile process")]
    Restart {
        name: String,
        #[arg(long)]
        force: bool,
    },
    #[command(about = "Show profile process status")]
    Status(ProfileStatusArgs),
    #[command(about = "Manage profile agent definitions")]
    Agent {
        #[command(subcommand)]
        command: ProfileAgentCliCommand,
    },
    #[command(about = "Manage profile supervisor workflows")]
    Workflow {
        #[command(subcommand)]
        command: ProfileWorkflowCliCommand,
    },
}

#[derive(Debug, Args)]
struct ProfileCreateArgs {
    #[arg(help = "Profile name to create under .remi-cat/profiles/<name>")]
    name: String,
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "KEY=VALUE",
        help = "Runtime config override, for example admin.enabled=false or acp.client=codex"
    )]
    entries: Vec<String>,
}

#[derive(Debug, Args)]
struct ProfileStatusArgs {
    name: Option<String>,
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Subcommand)]
enum ProfileAgentCliCommand {
    #[command(about = "List builtin and profile-specific agent definitions")]
    List {
        #[arg(help = "Runtime profile name")]
        profile: String,
    },
    #[command(about = "Show one resolved agent definition")]
    Show {
        #[arg(help = "Runtime profile name")]
        profile: String,
        #[arg(help = "Agent id from the Markdown frontmatter")]
        agent_id: String,
    },
    #[command(
        about = "Validate and copy an agent Markdown file into a profile",
        after_help = "Agent files use YAML frontmatter followed by the system prompt body. The command writes to `<profile-data-dir>/agents/<id>.md`."
    )]
    Upsert {
        #[arg(help = "Runtime profile name")]
        profile: String,
        #[arg(help = "Path to an agent Markdown file")]
        path: String,
    },
    #[command(about = "Set the profile root agent id")]
    SetDefault {
        #[arg(help = "Runtime profile name")]
        profile: String,
        #[arg(help = "Agent id to use as the profile root agent")]
        agent_id: String,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileWorkflowCliCommand {
    #[command(about = "List builtin and profile-specific supervisor workflows")]
    List {
        #[arg(help = "Runtime profile name")]
        profile: String,
    },
    #[command(about = "Show one resolved supervisor workflow")]
    Show {
        #[arg(help = "Runtime profile name")]
        profile: String,
        #[arg(help = "Workflow id")]
        workflow_id: String,
    },
    #[command(
        about = "Validate and copy a supervisor workflow JSON file into a profile",
        after_help = "Workflow files are JSON graph definitions. The command writes to `<profile-data-dir>/workflows/<id>.json`."
    )]
    Upsert {
        #[arg(help = "Runtime profile name")]
        profile: String,
        #[arg(help = "Path to a workflow JSON file")]
        path: String,
    },
    #[command(about = "Delete a profile-specific supervisor workflow")]
    Delete {
        #[arg(help = "Runtime profile name")]
        profile: String,
        #[arg(help = "Workflow id")]
        workflow_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FeishuCommand {
    Init,
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexCommand {
    Setup {
        bin: Option<String>,
        agent: Option<String>,
    },
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateCommand {
    Check {
        json: bool,
    },
    SelfUpdate {
        version: Option<String>,
        force: bool,
        dry_run: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct UpdateStatus {
    pub(crate) current_version: String,
    pub(crate) latest_version: String,
    pub(crate) latest_tag: String,
    pub(crate) update_available: bool,
    pub(crate) repo: String,
    pub(crate) git_url: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct GitHubRelease {
    pub(crate) tag_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedbackCommand {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) repo: Option<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) include_logs: bool,
    pub(crate) dry_run: bool,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct GitHubIssueCreateRequest {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) labels: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct GitHubIssueCreateResponse {
    pub(crate) html_url: String,
    pub(crate) number: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SecretCommand {
    List,
    Get(String),
    Set { key: String, value: String },
    Delete(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CliConfig {
    pub(crate) enabled: bool,
    pub(crate) tui: bool,
    pub(crate) resume: bool,
    pub(crate) resume_session_id: Option<String>,
    pub(crate) once: Option<String>,
    pub(crate) pure_prompt: bool,
    pub(crate) admin_only: bool,
    pub(crate) channel_id: String,
    pub(crate) user_id: String,
    pub(crate) username: String,
}

impl CliConfig {
    #[cfg(test)]
    pub(crate) fn from_args(args: &[String]) -> anyhow::Result<Self> {
        let mut enabled = args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--local" | "--cli-im" | "cli"));
        let mut tui = args.iter().any(|arg| matches!(arg.as_str(), "tui"));
        let mut resume = false;
        let mut resume_session_id = None;
        let mut once = None;
        let mut pure_prompt = false;
        let mut admin_only = args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--admin-only" | "admin"));
        let mut channel_id = CLI_CHAT_ID.to_string();
        let mut user_id = CLI_USER_ID.to_string();
        let mut username = CLI_USERNAME.to_string();

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "cli" => {
                    enabled = true;
                }
                "tui" => {
                    enabled = true;
                    tui = true;
                }
                "resume" if tui => {
                    resume = true;
                    if let Some(value) = optional_arg(args, i) {
                        resume_session_id = Some(value);
                        i += 1;
                    }
                }
                "prompt" => {
                    enabled = true;
                    pure_prompt = true;
                }
                "admin" | "--admin-only" => {
                    admin_only = true;
                }
                "-p" | "--prompt" => {
                    enabled = true;
                    pure_prompt = true;
                }
                "--cli-im-once" | "--cli-message" | "-m" => {
                    enabled = true;
                    if i + 1 >= args.len() {
                        anyhow::bail!("{} requires a message", args[i]);
                    }
                    once = Some(args[i + 1..].join(" "));
                    break;
                }
                "--cli-channel" | "--channel" | "--session" => {
                    channel_id = next_arg(args, i)?;
                    i += 1;
                }
                "--resume" => {
                    enabled = true;
                    tui = true;
                    resume = true;
                    if let Some(value) = optional_arg(args, i) {
                        resume_session_id = Some(value);
                        i += 1;
                    }
                }
                "--cli-user" | "--user" => {
                    user_id = next_arg(args, i)?;
                    i += 1;
                }
                "--cli-name" | "--name" => {
                    username = next_arg(args, i)?;
                    i += 1;
                }
                value if enabled && !value.starts_with('-') => {
                    once = Some(args[i..].join(" "));
                    break;
                }
                _ => {}
            }
            i += 1;
        }

        if pure_prompt && once.is_none() {
            anyhow::bail!("prompt mode requires a prompt");
        }

        Ok(Self {
            enabled,
            tui,
            resume,
            resume_session_id,
            once,
            pure_prompt,
            admin_only,
            channel_id,
            user_id,
            username,
        })
    }
}

#[cfg(test)]
pub(crate) fn parse_command(args: &[String]) -> anyhow::Result<AppCommand> {
    parse_cli_args(args).map(|parsed| parsed.command)
}

pub(crate) fn parse_cli_args(args: &[String]) -> anyhow::Result<GlobalArgsAndCommand> {
    let cli = try_parse_cli_args(args)?;
    let tool_output_overflow_bytes = cli.tool_output_overflow_bytes;
    let command = cli_command_to_app(cli.command, cli.run)?;
    Ok(GlobalArgsAndCommand {
        profile: cli.profile,
        tool_output_overflow_bytes,
        command,
    })
}

pub(crate) fn try_parse_cli_args(args: &[String]) -> Result<CliArgs, clap::Error> {
    let argv = std::iter::once("remi-cat".to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>();
    CliArgs::try_parse_from(argv)
}

#[derive(Debug)]
pub(crate) struct GlobalArgsAndCommand {
    pub(crate) profile: Option<String>,
    pub(crate) tool_output_overflow_bytes: Option<usize>,
    pub(crate) command: AppCommand,
}

fn validate_profile_arg(value: &str) -> Result<String, String> {
    instance_profile::validate_profile_name(value)
        .map(|_| value.to_string())
        .map_err(|err| err.to_string())
}

fn validate_positive_usize_arg(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|err| format!("invalid positive integer `{value}`: {err}"))?;
    if parsed == 0 {
        Err("value must be greater than 0".to_string())
    } else {
        Ok(parsed)
    }
}

fn cli_command_to_app(command: Option<CliCommand>, run: RunArgs) -> anyhow::Result<AppCommand> {
    match command {
        Some(CliCommand::Setup(args)) => Ok(AppCommand::Setup(args.entries)),
        Some(CliCommand::Doctor) => Ok(AppCommand::Doctor),
        Some(CliCommand::Tools(args)) => Ok(AppCommand::Tools(args)),
        Some(CliCommand::Hooks(args)) => Ok(AppCommand::Hooks(hooks_cli_to_command(args))),
        Some(CliCommand::Secrets(args)) => Ok(AppCommand::Secrets(secret_cli_to_command(args))),
        Some(CliCommand::Config(args)) => match args.command {
            ConfigCliCommand::Set(entries) => Ok(AppCommand::ConfigSet(entries.entries)),
        },
        Some(CliCommand::Sandbox(args)) => match args.command {
            SandboxCliCommand::Set(entries) => Ok(AppCommand::SandboxSet(entries.entries)),
        },
        Some(CliCommand::Profile { command }) => {
            Ok(AppCommand::Profile(profile_cli_to_command(command)?))
        }
        Some(CliCommand::Feishu { command }) => Ok(AppCommand::Feishu(match command {
            FeishuCliCommand::Init => FeishuCommand::Init,
            FeishuCliCommand::Doctor => FeishuCommand::Doctor,
        })),
        Some(CliCommand::Codex { command }) => Ok(AppCommand::Codex(match command {
            CodexCliCommand::Setup { bin, agent } => CodexCommand::Setup { bin, agent },
            CodexCliCommand::Doctor => CodexCommand::Doctor,
        })),
        Some(CliCommand::Update { command }) => Ok(AppCommand::Update(match command {
            UpdateCliCommand::Check { json } => UpdateCommand::Check { json },
            UpdateCliCommand::SelfUpdate {
                version,
                force,
                dry_run,
            } => UpdateCommand::SelfUpdate {
                version,
                force,
                dry_run,
            },
        })),
        Some(CliCommand::Feedback(args)) => {
            Ok(AppCommand::Feedback(feedback_args_to_command(args)?))
        }
        Some(CliCommand::Issue { command }) => match command {
            IssueCliCommand::Create(args) => {
                Ok(AppCommand::Feedback(feedback_args_to_command(args)?))
            }
        },
        Some(CliCommand::Cli(args)) => Ok(AppCommand::Run(local_chat_args_to_config(args))),
        Some(CliCommand::Tui(args)) => Ok(AppCommand::Run(tui_args_to_config(args))),
        Some(CliCommand::Prompt(args)) => Ok(AppCommand::Run(prompt_args_to_config(args))),
        Some(CliCommand::Admin) => Ok(AppCommand::Run(CliConfig {
            enabled: false,
            tui: false,
            resume: false,
            resume_session_id: None,
            once: None,
            pure_prompt: false,
            admin_only: true,
            channel_id: CLI_CHAT_ID.to_string(),
            user_id: CLI_USER_ID.to_string(),
            username: CLI_USERNAME.to_string(),
        })),
        None => Ok(AppCommand::Run(run_args_to_config(run)?)),
    }
}

fn hooks_cli_to_command(args: HooksArgs) -> HooksCommand {
    match args
        .command
        .unwrap_or(HooksCliCommand::List { json: false })
    {
        HooksCliCommand::List { json } => HooksCommand::List { json },
        HooksCliCommand::Trust { hash } => HooksCommand::Trust { hash },
        HooksCliCommand::Enable { hash } => HooksCommand::Enable { hash },
        HooksCliCommand::Disable { hash } => HooksCommand::Disable { hash },
    }
}

fn secret_cli_to_command(args: SecretsArgs) -> SecretCommand {
    match args.command.unwrap_or(SecretCliCommand::List) {
        SecretCliCommand::List => SecretCommand::List,
        SecretCliCommand::Get { key } => SecretCommand::Get(key),
        SecretCliCommand::Set { key, value } => SecretCommand::Set { key, value },
        SecretCliCommand::Delete { key } => SecretCommand::Delete(key),
    }
}

fn feedback_args_to_command(args: FeedbackArgs) -> anyhow::Result<FeedbackCommand> {
    let mut labels = if args.no_default_label {
        Vec::new()
    } else {
        vec!["feedback".to_string()]
    };
    labels.extend(args.labels);

    let positional_text = args.message.join(" ").trim().to_string();
    let title = args
        .title
        .or_else(|| {
            (!positional_text.is_empty()).then(|| feedback_title_from_text(&positional_text))
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("usage: remi-cat feedback --title <title> [--body <body>]")
        })?;
    let body = args
        .body
        .or_else(|| (!positional_text.is_empty()).then_some(positional_text))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| title.clone());
    labels.sort();
    labels.dedup();
    Ok(FeedbackCommand {
        title,
        body,
        repo: args.repo,
        labels,
        include_logs: args.include_logs,
        dry_run: args.dry_run,
    })
}

fn profile_cli_to_command(command: ProfileCliCommand) -> anyhow::Result<ProfileCommand> {
    match command {
        ProfileCliCommand::List => Ok(ProfileCommand::List),
        ProfileCliCommand::Show { name } => {
            let _ = InstanceProfile::from_label(&name)?;
            Ok(ProfileCommand::Show(name))
        }
        ProfileCliCommand::Create(args) => {
            if args.name == DIAGNOSTIC_PROFILE_NAME {
                anyhow::bail!(
                    "profile `{DIAGNOSTIC_PROFILE_NAME}` is builtin and cannot be created manually"
                );
            }
            instance_profile::validate_profile_name(&args.name)?;
            Ok(ProfileCommand::Create {
                name: args.name,
                entries: args.entries,
            })
        }
        ProfileCliCommand::Delete { name, force } => {
            if name == "default" {
                anyhow::bail!("the default profile cannot be deleted");
            }
            if name == DIAGNOSTIC_PROFILE_NAME {
                anyhow::bail!("builtin profile `{DIAGNOSTIC_PROFILE_NAME}` cannot be deleted");
            }
            instance_profile::validate_profile_name(&name)?;
            Ok(ProfileCommand::Delete { name, force })
        }
        ProfileCliCommand::Start { name } => {
            let _ = InstanceProfile::from_label(&name)?;
            Ok(ProfileCommand::Start(name))
        }
        ProfileCliCommand::Stop { name, force } => {
            let _ = InstanceProfile::from_label(&name)?;
            Ok(ProfileCommand::Stop { name, force })
        }
        ProfileCliCommand::Restart { name, force } => {
            let _ = InstanceProfile::from_label(&name)?;
            Ok(ProfileCommand::Restart { name, force })
        }
        ProfileCliCommand::Status(args) => {
            if args.all {
                Ok(ProfileCommand::StatusAll)
            } else {
                let name = args
                    .name
                    .ok_or_else(|| anyhow::anyhow!("usage: remi-cat profile status <profile>"))?;
                let _ = InstanceProfile::from_label(&name)?;
                Ok(ProfileCommand::Status(name))
            }
        }
        ProfileCliCommand::Agent { command } => Ok(ProfileCommand::Agent(match command {
            ProfileAgentCliCommand::List { profile } => {
                let _ = InstanceProfile::from_label(&profile)?;
                profile_command::ProfileAgentCommand::List { profile }
            }
            ProfileAgentCliCommand::Show { profile, agent_id } => {
                let _ = InstanceProfile::from_label(&profile)?;
                profile_command::ProfileAgentCommand::Show { profile, agent_id }
            }
            ProfileAgentCliCommand::Upsert { profile, path } => {
                let _ = InstanceProfile::from_label(&profile)?;
                profile_command::ProfileAgentCommand::Upsert { profile, path }
            }
            ProfileAgentCliCommand::SetDefault { profile, agent_id } => {
                let _ = InstanceProfile::from_label(&profile)?;
                profile_command::ProfileAgentCommand::SetDefault { profile, agent_id }
            }
        })),
        ProfileCliCommand::Workflow { command } => Ok(ProfileCommand::Workflow(match command {
            ProfileWorkflowCliCommand::List { profile } => {
                let _ = InstanceProfile::from_label(&profile)?;
                profile_command::ProfileWorkflowCommand::List { profile }
            }
            ProfileWorkflowCliCommand::Show {
                profile,
                workflow_id,
            } => {
                let _ = InstanceProfile::from_label(&profile)?;
                profile_command::validate_file_id(&workflow_id)?;
                profile_command::ProfileWorkflowCommand::Show {
                    profile,
                    workflow_id,
                }
            }
            ProfileWorkflowCliCommand::Upsert { profile, path } => {
                let _ = InstanceProfile::from_label(&profile)?;
                profile_command::ProfileWorkflowCommand::Upsert { profile, path }
            }
            ProfileWorkflowCliCommand::Delete {
                profile,
                workflow_id,
            } => {
                let _ = InstanceProfile::from_label(&profile)?;
                profile_command::validate_file_id(&workflow_id)?;
                if workflow_id == "goal" {
                    anyhow::bail!("embedded workflow `goal` cannot be deleted");
                }
                profile_command::ProfileWorkflowCommand::Delete {
                    profile,
                    workflow_id,
                }
            }
        })),
    }
}

fn local_chat_args_to_config(args: LocalChatArgs) -> CliConfig {
    CliConfig {
        enabled: true,
        tui: false,
        resume: false,
        resume_session_id: None,
        once: (!args.message.is_empty()).then(|| args.message.join(" ")),
        pure_prompt: false,
        admin_only: false,
        channel_id: args.common.channel_id,
        user_id: args.common.user_id,
        username: args.common.username,
    }
}

fn tui_args_to_config(args: TuiArgs) -> CliConfig {
    let mut channel_id = args.common.channel_id;
    let mut user_id = args.common.user_id;
    let mut username = args.common.username;
    let (resume, resume_session_id) = match args.command {
        Some(TuiCliCommand::Resume {
            session_id,
            channel_id: resume_channel_id,
            user_id: resume_user_id,
            username: resume_username,
        }) => {
            if let Some(value) = resume_channel_id {
                channel_id = value;
            }
            if let Some(value) = resume_user_id {
                user_id = value;
            }
            if let Some(value) = resume_username {
                username = value;
            }
            (true, session_id)
        }
        None => (false, None),
    };
    CliConfig {
        enabled: true,
        tui: true,
        resume,
        resume_session_id,
        once: None,
        pure_prompt: false,
        admin_only: false,
        channel_id,
        user_id,
        username,
    }
}

fn prompt_args_to_config(args: PromptArgs) -> CliConfig {
    CliConfig {
        enabled: true,
        tui: false,
        resume: false,
        resume_session_id: None,
        once: Some(args.prompt.join(" ")),
        pure_prompt: true,
        admin_only: false,
        channel_id: args.common.channel_id,
        user_id: args.common.user_id,
        username: args.common.username,
    }
}

fn run_args_to_config(args: RunArgs) -> anyhow::Result<CliConfig> {
    let pure_prompt = !args.prompt.is_empty();
    let once = if pure_prompt {
        Some(args.prompt.join(" "))
    } else if !args.cli_message.is_empty() {
        Some(args.cli_message.join(" "))
    } else {
        None
    };
    let resume = args.resume.is_some();
    Ok(CliConfig {
        enabled: args.local || args.cli_im || args.cli_message.len() > 0 || pure_prompt,
        tui: resume,
        resume,
        resume_session_id: args.resume.flatten(),
        once,
        pure_prompt,
        admin_only: args.admin_only,
        channel_id: args.channel_id,
        user_id: args.user_id,
        username: args.username,
    })
}

pub(crate) fn parse_secret_command(args: &[String]) -> anyhow::Result<SecretCommand> {
    match args.first().map(String::as_str) {
        Some("list") | None => Ok(SecretCommand::List),
        Some("get") => Ok(SecretCommand::Get(next_arg(args, 0)?)),
        Some("set") => {
            let key = next_arg(args, 0)?;
            let value = args
                .get(2)
                .map(String::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if value.is_empty() {
                anyhow::bail!("usage: remi-cat secrets set <KEY> <VALUE>");
            }
            Ok(SecretCommand::Set { key, value })
        }
        Some("delete") | Some("remove") | Some("unset") => {
            Ok(SecretCommand::Delete(next_arg(args, 0)?))
        }
        Some(other) => anyhow::bail!("unknown `remi-cat secrets` subcommand `{other}`"),
    }
}

#[cfg(test)]
pub(crate) fn parse_global_args(args: &[String]) -> anyhow::Result<GlobalArgs> {
    let mut profile = None;
    let mut command_args = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" => {
                let value = next_arg(args, i)?;
                instance_profile::validate_profile_name(&value)?;
                if profile.replace(value).is_some() {
                    anyhow::bail!("--profile may only be specified once");
                }
                i += 2;
            }
            value if value.starts_with("--profile=") => {
                let value = value.trim_start_matches("--profile=").to_string();
                instance_profile::validate_profile_name(&value)?;
                if profile.replace(value).is_some() {
                    anyhow::bail!("--profile may only be specified once");
                }
                i += 1;
            }
            _ => {
                command_args.push(args[i].clone());
                i += 1;
            }
        }
    }
    Ok(GlobalArgs {
        profile,
        command_args,
    })
}

fn feedback_title_from_text(value: &str) -> String {
    let mut out = value
        .split_whitespace()
        .take(12)
        .collect::<Vec<_>>()
        .join(" ");
    if out.chars().count() > 80 {
        out = out.chars().take(77).collect::<String>();
        out.push_str("...");
    }
    out
}

fn next_arg(args: &[String], index: usize) -> anyhow::Result<String> {
    args.get(index + 1)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{} requires a value", args[index]))
}

#[cfg(test)]
fn optional_arg(args: &[String], index: usize) -> Option<String> {
    args.get(index + 1)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .map(ToOwned::to_owned)
}
