use crate::app::{CLI_CHAT_ID, CLI_USERNAME};
use crate::instance_profile::{self, InstanceProfile, DIAGNOSTIC_PROFILE_NAME};
use crate::profile_command::{self, ProfileCommand, ProfileWorkflowCommand};
use clap::{ArgAction, Args, Parser, Subcommand};

pub(crate) const CLI_USER_ID: &str = "local-user";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AppCommand {
    Run(CliConfig),
    Setup(Vec<String>),
    Doctor,
    Tools(ToolsArgs),
    Tasks(TasksCommand),
    Hooks(HooksCommand),
    Secrets(SecretCommand),
    ConfigSet(Vec<String>),
    SandboxSet(Vec<String>),
    Profile(ProfileCommand),
    A2a(A2aCommand),
    Feishu(FeishuCommand),
    Acp(AcpCommand),
    AcpAdapter(AcpAdapterCommand),
    Codex(CodexCommand),
    Update(UpdateCommand),
    Feedback(FeedbackCommand),
    Telemetry(TelemetryCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum A2aCommand {
    Stdio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TelemetryCommand {
    Status,
    Enable,
    Disable,
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
        value_name = "PROFILE_REF",
        value_parser = validate_profile_arg,
        help = "Select default, @alias, id:<id>, a legacy name, or a profile.yaml path"
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

    #[arg(
        long,
        global = true,
        help = "Disable automatic telemetry for this process"
    )]
    pub(crate) no_telemetry: bool,

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

    #[arg(
        long = "wait-background-tasks",
        help = "In one-shot CLI/prompt mode, wait for background tool tasks to finish before exiting"
    )]
    wait_background_tasks: bool,

    #[arg(long, value_parser = ["low", "medium"], help = "Auto-approve tool requests up to this risk level for this one-shot local session")]
    permissions: Option<String>,
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
    #[command(about = "List, inspect, or cancel background tool tasks")]
    Tasks(TasksArgs),
    #[command(about = "List, trust, enable, or disable Remi hooks")]
    Hooks(HooksArgs),
    #[command(alias = "secret", about = "List, read, set, or delete secrets")]
    Secrets(SecretsArgs),
    #[command(about = "Update runtime config")]
    Config(ConfigArgs),
    #[command(about = "Update sandbox runtime config")]
    Sandbox(SandboxArgs),
    #[command(about = "Show, enable, or disable telemetry for the selected profile")]
    Telemetry {
        #[command(subcommand)]
        command: TelemetryCliCommand,
    },
    #[command(
        about = "Create, register, inspect, validate, and run Application Profiles",
        long_about = "Application Profiles are project-level configuration and resource assembly manifests. They reference resources; they do not own or isolate them.",
        after_help = "Start here:\n  remi-cat profile current\n  remi-cat profile list\n  remi-cat profile show @travel --resolved\n  remi-cat profile check @travel\n\nReferences:\n  default | @alias | id:stable.id | ./path/profile.yaml | legacy-name\n\nUse the positional PROFILE_REF to operate on a target. The global --profile option selects the current profile when a command omits its target."
    )]
    Profile {
        #[command(subcommand)]
        command: ProfileCliCommand,
    },
    #[command(about = "Serve the selected profile over the A2A protocol")]
    A2a {
        #[command(subcommand)]
        command: A2aCliCommand,
    },
    #[command(about = "Manage supervisor workflows")]
    Workflow {
        #[command(subcommand)]
        command: WorkflowCliCommand,
    },
    #[command(about = "Manage Feishu/Lark setup")]
    Feishu {
        #[command(subcommand)]
        command: FeishuCliCommand,
    },
    #[command(about = "Configure and inspect ACP clients")]
    Acp {
        #[command(subcommand)]
        command: AcpCliCommand,
    },
    #[command(about = "Run built-in ACP adapter processes")]
    AcpAdapter {
        #[command(subcommand)]
        command: AcpAdapterCliCommand,
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
    #[command(about = "Send feedback to Sentry")]
    Feedback(FeedbackArgs),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TasksCommand {
    List { json: bool },
    Get { task_id: String, json: bool },
    Cancel { task_id: String, json: bool },
}

#[derive(Debug, Args)]
struct TasksArgs {
    #[command(subcommand)]
    command: Option<TasksCliCommand>,
}

#[derive(Debug, Subcommand)]
enum TasksCliCommand {
    #[command(about = "List background tool tasks")]
    List {
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Show one background tool task")]
    Get {
        task_id: String,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
    #[command(about = "Cancel one background tool task")]
    Cancel {
        task_id: String,
        #[arg(long, help = "Print machine-readable JSON")]
        json: bool,
    },
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
        about = "Configure the built-in Codex ACP adapter profile",
        after_help = "Examples:\n  remi-cat codex setup\n  remi-cat codex setup --bin /usr/local/bin/codex --agent default\n  remi-cat codex setup --arg=--config --arg=model=\\\"gpt-5-codex\\\"\n\nThis writes a local ACP profile that launches `remi-cat acp-adapter codex`. `--bin` and repeated `--arg` values are stored as adapter argv, not as backend-specific Codex settings."
    )]
    Setup {
        #[arg(
            long = "bin",
            value_name = "PATH",
            help = "Path to the codex binary used by the adapter"
        )]
        bin: Option<String>,
        #[arg(long, value_name = "NAME", help = "ACP agent name")]
        agent: Option<String>,
        #[arg(
            long = "arg",
            value_name = "ARG",
            action = ArgAction::Append,
            allow_hyphen_values = true,
            help = "Extra Codex startup arg passed by the adapter before `exec`; repeat to pass multiple args"
        )]
        args: Vec<String>,
    },
    #[command(about = "Inspect the configured Codex ACP adapter profile")]
    Doctor,
}

#[derive(Debug, Subcommand)]
enum AcpCliCommand {
    #[command(
        about = "Configure an ACP client",
        after_help = "Examples:\n  remi-cat acp setup --client codex --bin /usr/local/bin/codex\n  remi-cat acp setup --client remi --tool-name acp__remi\n  remi-cat acp setup --client remi --bin /path/to/remi-cat --tool-name acp__remi\n  remi-cat acp setup --client my-acp --mode remote --base-url http://127.0.0.1:8788 --tool-name acp__my_acp\n\nThis writes generic `acp.*` runtime settings to the selected profile runtime config. `--client codex` uses the bundled `acp-adapter codex` profile so the backend still talks standard ACP stdio."
    )]
    Setup {
        #[arg(
            long,
            default_value = "codex",
            value_name = "NAME",
            help = "ACP client id"
        )]
        client: String,
        #[arg(long, value_name = "local|remote", help = "ACP mode")]
        mode: Option<String>,
        #[arg(
            long = "tool-name",
            value_name = "NAME",
            help = "Tool name exposed to agents"
        )]
        tool_name: Option<String>,
        #[arg(long, value_name = "NAME", help = "ACP agent name")]
        agent: Option<String>,
        #[arg(long = "base-url", value_name = "URL", help = "Remote ACP base URL")]
        base_url: Option<String>,
        #[arg(long, value_name = "MODEL", help = "Remote model name")]
        model: Option<String>,
        #[arg(
            long = "api-key",
            value_name = "KEY",
            help = "Remote ACP bearer API key"
        )]
        api_key: Option<String>,
        #[arg(long = "bin", value_name = "PATH", help = "Local ACP client binary")]
        bin: Option<String>,
        #[arg(
            long = "arg",
            value_name = "ARG",
            action = ArgAction::Append,
            allow_hyphen_values = true,
            help = "Extra local ACP startup arg; repeat to pass multiple args"
        )]
        args: Vec<String>,
    },
    #[command(about = "Inspect ACP configuration")]
    Doctor,
    #[command(
        about = "Run remi-cat as a standard ACP stdio agent",
        long_about = "Run remi-cat as an Agent Client Protocol stdio agent. stdin/stdout are reserved for ACP JSON-RPC; logs are emitted on stderr."
    )]
    Agent,
}

#[derive(Debug, Subcommand)]
enum AcpAdapterCliCommand {
    #[command(
        about = "Run Codex through a standard ACP stdio adapter",
        after_help = "Examples:\n  remi-cat acp-adapter codex\n  remi-cat acp-adapter codex --bin /usr/local/bin/codex --arg=--config --arg=model=\\\"gpt-5-codex\\\"\n\nThe adapter speaks ACP on stdin/stdout and translates each prompt into `codex exec --json`."
    )]
    Codex {
        #[arg(long = "bin", value_name = "PATH", help = "Path to the codex binary")]
        bin: Option<String>,
        #[arg(
            long = "arg",
            value_name = "ARG",
            action = ArgAction::Append,
            allow_hyphen_values = true,
            help = "Extra Codex startup arg inserted before `exec`; repeat to pass multiple args"
        )]
        args: Vec<String>,
    },
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
        about = "Install a selected remi-cat release",
        after_help = "Examples:\n  remi-cat update self --dry-run\n  remi-cat update self --binary\n  remi-cat update self --binary --version v0.2.1\n\nBy default the installer uses `cargo install`. Pass `--binary` to download the platform release asset and replace the local executable."
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
        #[arg(long, help = "Print the installation plan without running it")]
        dry_run: bool,
        #[arg(
            long,
            help = "Download the platform release binary instead of building with cargo install"
        )]
        binary: bool,
    },
}

#[derive(Debug, Args)]
struct FeedbackArgs {
    #[arg(short, long)]
    title: Option<String>,
    #[arg(short, long)]
    pub(crate) body: Option<String>,
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
enum TelemetryCliCommand {
    Status,
    Enable,
    Disable,
}

#[derive(Debug, Args)]
struct LocalChatArgs {
    #[command(flatten)]
    common: LocalCommonArgs,
    #[arg(
        long = "wait-background-tasks",
        help = "When MESSAGE is provided, wait for background tool tasks to finish before exiting"
    )]
    wait_background_tasks: bool,
    #[arg(long, value_parser = ["low", "medium"], help = "Auto-approve tool requests up to this risk level for this local session")]
    permissions: Option<String>,
    #[arg(trailing_var_arg = true, value_name = "MESSAGE")]
    message: Vec<String>,
}

#[derive(Debug, Args)]
struct PromptArgs {
    #[command(flatten)]
    common: LocalCommonArgs,
    #[arg(
        long = "wait-background-tasks",
        help = "Wait for background tool tasks to finish before exiting"
    )]
    wait_background_tasks: bool,
    #[arg(long, value_parser = ["low", "medium"], help = "Auto-approve tool requests up to this risk level for this prompt session; use medium only for explicitly authorized automation")]
    permissions: Option<String>,
    #[arg(required = true, trailing_var_arg = true, value_name = "PROMPT")]
    prompt: Vec<String>,
}

#[derive(Debug, Args)]
struct TuiArgs {
    #[command(flatten)]
    common: LocalCommonArgs,
    #[arg(
        long = "async",
        help = "Enable automatic async-agent handling for background tool tasks (default for TUI)"
    )]
    async_agent: bool,
    #[arg(
        long = "sync",
        help = "Disable async-agent handling; wait synchronously for tool tasks",
        conflicts_with = "async_agent"
    )]
    sync: bool,
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
    #[command(about = "Explain which profile is currently selected and why")]
    Current,
    #[command(about = "List builtin, registered, and legacy profiles")]
    List(ProfileListArgs),
    #[command(about = "Find profiles by declared capabilities")]
    Find(ProfileFindArgs),
    #[command(about = "Show declared and resolved profile configuration")]
    Show(ProfileShowArgs),
    #[command(about = "Validate a profile manifest, references, resources, and A2A endpoint")]
    Check(ProfileCheckArgs),
    #[command(about = "Create a new profile.yaml manifest")]
    Init(ProfileInitArgs),
    #[command(about = "Register a profile manifest by reference; no files are copied")]
    Register(ProfileRegisterArgs),
    #[command(about = "Remove a registry alias; manifest and resources are preserved")]
    Unregister { reference: String },
    #[command(about = "Set one typed manifest field")]
    Set(ProfileSetArgs),
    #[command(about = "Remove one optional manifest field")]
    Unset(ProfileUnsetArgs),
    #[command(about = "Start a local profile on demand and ask it through A2A")]
    Ask(ProfileAskArgs),
    #[command(about = "Start a persistent managed runtime instance for a profile")]
    Start(ProfileLaunchArgs),
    #[command(about = "Stop a persistent managed runtime instance")]
    Stop(ProfileControlArgs),
    #[command(about = "Restart a persistent managed runtime instance")]
    Restart(ProfileControlArgs),
    #[command(about = "Show persistent managed runtime instance state")]
    Status(ProfileStatusArgs),
    #[command(about = "Configure concrete IM channel instances for a profile")]
    Channel {
        #[command(subcommand)]
        command: ProfileChannelCliCommand,
    },
    #[command(about = "Inspect referenced profile resources")]
    Resource {
        #[command(subcommand)]
        command: ProfileResourceCliCommand,
    },
    #[command(about = "Inspect and repair the cross-process profile registry")]
    Registry {
        #[command(subcommand)]
        command: ProfileRegistryCliCommand,
    },
    #[command(hide = true, about = "Deprecated: use profile init/register/setup")]
    Create(ProfileCreateArgs),
    #[command(hide = true, about = "Deprecated: use profile unregister")]
    Delete {
        name: String,
        #[arg(long)]
        force: bool,
    },
    #[command(hide = true, about = "Deprecated: use remi-cat agent --profile")]
    Agent {
        #[command(subcommand)]
        command: ProfileAgentCliCommand,
    },
    #[command(hide = true, about = "Deprecated: use remi-cat workflow --profile")]
    Workflow {
        #[command(subcommand)]
        command: ProfileWorkflowCliCommand,
    },
}

#[derive(Debug, Subcommand)]
enum A2aCliCommand {
    #[command(about = "Serve framed A2A messages over stdin/stdout")]
    Stdio,
}

#[derive(Debug, Args)]
struct ProfileAskArgs {
    #[arg(value_name = "PROFILE_REF")]
    reference: String,
    #[arg(long, default_value = "default")]
    named: String,
    #[arg(long)]
    agent_id: Option<String>,
    #[arg(num_args = 1.., trailing_var_arg = true, value_name = "TASK")]
    task: Vec<String>,
}

#[derive(Debug, Args)]
struct ProfileListArgs {
    #[arg(long, default_value = "all", value_parser = ["all", "registered", "legacy"], help = "Select all profiles, registry entries only, or builtin/legacy profiles only")]
    scope: String,
    #[arg(long, default_value = "table", value_parser = ["table", "plain", "json", "yaml"], help = "Choose human or machine-readable output")]
    format: String,
}

#[derive(Debug, Args)]
struct ProfileFindArgs {
    #[arg(
        long,
        help = "Require a declared capability tag; repeat for AND matching"
    )]
    tag: Vec<String>,
    #[arg(long, help = "Require a declared intent; repeat for AND matching")]
    intent: Vec<String>,
    #[arg(long, help = "Require a declared channel")]
    channel: Option<String>,
    #[arg(long, default_value = "table", value_parser = ["table", "plain", "json", "yaml"], help = "Choose human or machine-readable output")]
    format: String,
}

#[derive(Debug, Args)]
struct ProfileShowArgs {
    #[arg(
        value_name = "PROFILE_REF",
        help = "Target profile; defaults to the globally selected/current profile"
    )]
    reference: Option<String>,
    #[arg(long, conflicts_with_all = ["resolved", "sources"], help = "Print the declared manifest without resolving paths")]
    manifest: bool,
    #[arg(long, conflicts_with_all = ["manifest", "sources"], help = "Print the effective paths and endpoint")]
    resolved: bool,
    #[arg(long, conflicts_with_all = ["manifest", "resolved"], help = "Explain each field's declared or default source")]
    sources: bool,
    #[arg(long, default_value = "plain", value_parser = ["plain", "json", "yaml"], help = "Choose human or machine-readable output")]
    format: String,
}

#[derive(Debug, Args)]
struct ProfileCheckArgs {
    #[arg(
        value_name = "PROFILE_REF",
        help = "Target profile; defaults to the globally selected/current profile"
    )]
    reference: Option<String>,
    #[arg(long, help = "Treat warnings as validation failures")]
    strict: bool,
    #[arg(long, default_value = "plain", value_parser = ["plain", "json"], help = "Choose human or machine-readable output")]
    format: String,
}

#[derive(Debug, Args)]
struct ProfileInitArgs {
    #[arg(
        default_value = ".",
        value_name = "DIRECTORY",
        help = "Directory to contain profile.yaml, or an explicit .yaml path"
    )]
    directory: String,
    #[arg(long, help = "Stable manifest ID; defaults to the directory name")]
    id: Option<String>,
    #[arg(long, help = "Human-readable name; defaults to the directory name")]
    name: Option<String>,
    #[arg(long, default_value = "remi-cat", value_parser = ["minimal", "remi-cat", "external"], help = "Select the initial resource and state layout")]
    template: String,
    #[arg(
        long,
        value_name = "ALIAS",
        help = "Also register the new manifest as @ALIAS"
    )]
    register: Option<String>,
    #[arg(
        long,
        help = "Materialize remi-cat runtime config and builtin agent/model resources"
    )]
    with_runtime: bool,
}

#[derive(Debug, Args)]
struct ProfileRegisterArgs {
    #[arg(
        value_name = "PROFILE_YAML",
        help = "Manifest file or its containing directory"
    )]
    path: String,
    #[arg(long, help = "Registry alias; defaults to a normalized manifest ID")]
    alias: Option<String>,
    #[arg(long, help = "Replace an existing alias mapping")]
    replace: bool,
}

#[derive(Debug, Args)]
struct ProfileSetArgs {
    #[arg(help = "Profile manifest to edit")]
    reference: String,
    #[arg(help = "Typed dotted manifest field, for example resources.skills")]
    field: String,
    #[arg(help = "New value; list fields accept comma-separated or JSON-array syntax")]
    value: String,
    #[arg(long, help = "Print the resulting manifest without writing it")]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct ProfileUnsetArgs {
    #[arg(help = "Profile manifest to edit")]
    reference: String,
    #[arg(help = "Optional dotted manifest field to remove or reset")]
    field: String,
    #[arg(long, help = "Print the resulting manifest without writing it")]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct ProfileLaunchArgs {
    #[arg(value_name = "PROFILE_REF", help = "Profile to launch")]
    reference: String,
    #[arg(
        long,
        default_value = "default",
        help = "Independent runtime instance name"
    )]
    instance: String,
}

#[derive(Debug, Args)]
struct ProfileControlArgs {
    #[arg(value_name = "PROFILE_REF", help = "Profile containing the instance")]
    reference: String,
    #[arg(long, default_value = "default", help = "Runtime instance name")]
    instance: String,
    #[arg(long, help = "Force termination instead of requesting a graceful stop")]
    force: bool,
}

#[derive(Debug, Args)]
struct ProfileCreateArgs {
    #[arg(help = "Profile name to create under .remi-cat/profiles/<name>")]
    name: String,
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "KEY=VALUE",
        help = "Runtime config override, for example admin.enabled=true or acp.client=codex"
    )]
    entries: Vec<String>,
}

#[derive(Debug, Args)]
struct ProfileStatusArgs {
    #[arg(
        value_name = "PROFILE_REF",
        conflicts_with = "all",
        help = "Target profile; required unless --all is used"
    )]
    reference: Option<String>,
    #[arg(
        long,
        conflicts_with = "reference",
        help = "Show all managed instances"
    )]
    all: bool,
    #[arg(long, help = "Filter to one instance name")]
    instance: Option<String>,
    #[arg(long, default_value = "plain", value_parser = ["plain", "json"], help = "Choose human or machine-readable output")]
    format: String,
}

#[derive(Debug, Subcommand)]
enum ProfileChannelCliCommand {
    #[command(about = "List concrete channel instances and their effective state")]
    List {
        reference: String,
        #[arg(long, default_value = "plain", value_parser = ["plain", "json", "yaml"])]
        format: String,
    },
    #[command(about = "Create or replace a Feishu/Lark channel instance")]
    UpsertFeishu {
        reference: String,
        #[arg(value_name = "INSTANCE_ID")]
        id: String,
        #[arg(long, help = "Create the channel disabled")]
        disabled: bool,
        #[arg(long, default_value = "websocket", value_parser = ["websocket", "event-hook"])]
        transport: String,
        #[arg(long, default_value = "FEISHU_APP_ID")]
        app_id_env: String,
        #[arg(long, default_value = "FEISHU_APP_SECRET")]
        app_secret_env: String,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 8788)]
        port: u16,
        #[arg(long, default_value = "/feishu/events")]
        path: String,
        #[arg(long)]
        verification_token_env: Option<String>,
    },
    #[command(about = "Enable one configured channel instance")]
    Enable { reference: String, id: String },
    #[command(about = "Disable one configured channel instance without deleting it")]
    Disable { reference: String, id: String },
    #[command(about = "Remove one configured channel instance")]
    Remove {
        reference: String,
        id: String,
        #[arg(long, help = "Confirm destructive removal")]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ProfileResourceCliCommand {
    #[command(about = "List all resolved resource and state references")]
    List { reference: String },
    #[command(about = "Show one resolved resource reference")]
    Show { reference: String, resource: String },
    #[command(about = "Validate referenced resource files and directories")]
    Check { reference: String },
}

#[derive(Debug, Subcommand)]
enum ProfileRegistryCliCommand {
    #[command(about = "Show registry path, schema, entry count, and health")]
    Info,
    #[command(about = "List raw registry entries")]
    List,
    #[command(about = "Remove registry entries whose manifests no longer exist")]
    Repair,
    #[command(about = "Revalidate and rewrite the registry index")]
    RebuildIndex,
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

#[derive(Debug, Subcommand)]
enum WorkflowCliCommand {
    #[command(about = "List supervisor workflows")]
    List(WorkflowProfileArg),
    #[command(about = "Show one supervisor workflow")]
    Show {
        #[command(flatten)]
        profile: WorkflowProfileArg,
        #[arg(help = "Workflow id")]
        workflow_id: String,
    },
    #[command(
        alias = "upsert",
        about = "Validate and save a supervisor workflow JSON file"
    )]
    Add {
        #[command(flatten)]
        profile: WorkflowProfileArg,
        #[arg(help = "Path to a workflow JSON file")]
        path: String,
    },
    #[command(
        alias = "delete",
        about = "Delete a profile-specific supervisor workflow"
    )]
    Rm {
        #[command(flatten)]
        profile: WorkflowProfileArg,
        #[arg(help = "Workflow id")]
        workflow_id: String,
    },
}

#[derive(Debug, Args)]
struct WorkflowProfileArg {
    #[arg(long, default_value = "default", help = "Runtime profile name")]
    profile: String,
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
        args: Vec<String>,
    },
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcpCommand {
    Setup {
        client: String,
        mode: Option<String>,
        tool_name: Option<String>,
        agent: Option<String>,
        base_url: Option<String>,
        model: Option<String>,
        api_key: Option<String>,
        bin: Option<String>,
        args: Vec<String>,
    },
    Doctor,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcpAdapterCommand {
    Codex {
        bin: Option<String>,
        args: Vec<String>,
    },
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
        binary: bool,
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
    pub(crate) labels: Vec<String>,
    pub(crate) include_logs: bool,
    pub(crate) dry_run: bool,
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
    pub(crate) wait_background_tasks: bool,
    pub(crate) async_agent: bool,
    pub(crate) permissions: Option<String>,
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
        let mut wait_background_tasks = false;
        let mut permissions = None;
        // TUI defaults to async-agent mode unless --sync is provided.
        let mut async_agent = tui;
        let mut sync = false;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "cli" => {
                    enabled = true;
                }
                "tui" => {
                    enabled = true;
                    tui = true;
                    if !sync {
                        async_agent = true;
                    }
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
                    if !sync {
                        async_agent = true;
                    }
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
                "--wait-background-tasks" => {
                    wait_background_tasks = true;
                }
                "--permissions" => {
                    let value = next_arg(args, i)?;
                    if !matches!(value.as_str(), "low" | "medium") {
                        anyhow::bail!("--permissions must be low or medium");
                    }
                    permissions = Some(value);
                    i += 1;
                }
                "--async" => {
                    async_agent = true;
                    sync = false;
                }
                "--sync" => {
                    async_agent = false;
                    sync = true;
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
            wait_background_tasks,
            async_agent,
            permissions,
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
    let no_telemetry = cli.no_telemetry;
    let command = cli_command_to_app(cli.command, cli.run)?;
    Ok(GlobalArgsAndCommand {
        profile: cli.profile,
        tool_output_overflow_bytes,
        no_telemetry,
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
    pub(crate) no_telemetry: bool,
    pub(crate) command: AppCommand,
}

fn validate_profile_arg(value: &str) -> Result<String, String> {
    if let Some(alias) = value.strip_prefix('@') {
        return instance_profile::validate_profile_name(alias)
            .map(|_| value.to_string())
            .map_err(|err| err.to_string());
    }
    if let Some(id) = value.strip_prefix("id:") {
        if id.is_empty() {
            return Err("profile id after `id:` must not be empty".to_string());
        }
        return Ok(value.to_string());
    }
    if value.ends_with(".yaml")
        || value.ends_with(".yml")
        || value.contains('/')
        || value.contains('\\')
        || std::path::Path::new(value).is_dir()
    {
        return Ok(value.to_string());
    }
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
        Some(CliCommand::Tasks(args)) => Ok(AppCommand::Tasks(tasks_cli_to_command(args))),
        Some(CliCommand::Hooks(args)) => Ok(AppCommand::Hooks(hooks_cli_to_command(args))),
        Some(CliCommand::Secrets(args)) => Ok(AppCommand::Secrets(secret_cli_to_command(args))),
        Some(CliCommand::Config(args)) => match args.command {
            ConfigCliCommand::Set(entries) => Ok(AppCommand::ConfigSet(entries.entries)),
        },
        Some(CliCommand::Sandbox(args)) => match args.command {
            SandboxCliCommand::Set(entries) => Ok(AppCommand::SandboxSet(entries.entries)),
        },
        Some(CliCommand::Telemetry { command }) => Ok(AppCommand::Telemetry(match command {
            TelemetryCliCommand::Status => TelemetryCommand::Status,
            TelemetryCliCommand::Enable => TelemetryCommand::Enable,
            TelemetryCliCommand::Disable => TelemetryCommand::Disable,
        })),
        Some(CliCommand::Profile { command }) => {
            Ok(AppCommand::Profile(profile_cli_to_command(command)?))
        }
        Some(CliCommand::A2a { command }) => Ok(AppCommand::A2a(match command {
            A2aCliCommand::Stdio => A2aCommand::Stdio,
        })),
        Some(CliCommand::Workflow { command }) => Ok(AppCommand::Profile(
            ProfileCommand::Workflow(workflow_cli_to_command(command)?),
        )),
        Some(CliCommand::Feishu { command }) => Ok(AppCommand::Feishu(match command {
            FeishuCliCommand::Init => FeishuCommand::Init,
            FeishuCliCommand::Doctor => FeishuCommand::Doctor,
        })),
        Some(CliCommand::Acp { command }) => Ok(AppCommand::Acp(match command {
            AcpCliCommand::Setup {
                client,
                mode,
                tool_name,
                agent,
                base_url,
                model,
                api_key,
                bin,
                args,
            } => AcpCommand::Setup {
                client,
                mode,
                tool_name,
                agent,
                base_url,
                model,
                api_key,
                bin,
                args,
            },
            AcpCliCommand::Doctor => AcpCommand::Doctor,
            AcpCliCommand::Agent => AcpCommand::Agent,
        })),
        Some(CliCommand::AcpAdapter { command }) => Ok(AppCommand::AcpAdapter(match command {
            AcpAdapterCliCommand::Codex { bin, args } => AcpAdapterCommand::Codex { bin, args },
        })),
        Some(CliCommand::Codex { command }) => Ok(AppCommand::Codex(match command {
            CodexCliCommand::Setup { bin, agent, args } => CodexCommand::Setup { bin, agent, args },
            CodexCliCommand::Doctor => CodexCommand::Doctor,
        })),
        Some(CliCommand::Update { command }) => Ok(AppCommand::Update(match command {
            UpdateCliCommand::Check { json } => UpdateCommand::Check { json },
            UpdateCliCommand::SelfUpdate {
                version,
                force,
                dry_run,
                binary,
            } => UpdateCommand::SelfUpdate {
                version,
                force,
                dry_run,
                binary,
            },
        })),
        Some(CliCommand::Feedback(args)) => {
            Ok(AppCommand::Feedback(feedback_args_to_command(args)?))
        }
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
            wait_background_tasks: false,
            async_agent: false,
            permissions: None,
        })),
        None => Ok(AppCommand::Run(run_args_to_config(run)?)),
    }
}

fn workflow_cli_to_command(command: WorkflowCliCommand) -> anyhow::Result<ProfileWorkflowCommand> {
    match command {
        WorkflowCliCommand::List(args) => {
            let _ = InstanceProfile::from_label(&args.profile)?;
            Ok(ProfileWorkflowCommand::List {
                profile: args.profile,
            })
        }
        WorkflowCliCommand::Show {
            profile,
            workflow_id,
        } => {
            let _ = InstanceProfile::from_label(&profile.profile)?;
            profile_command::validate_file_id(&workflow_id)?;
            Ok(ProfileWorkflowCommand::Show {
                profile: profile.profile,
                workflow_id,
            })
        }
        WorkflowCliCommand::Add { profile, path } => {
            let _ = InstanceProfile::from_label(&profile.profile)?;
            Ok(ProfileWorkflowCommand::Upsert {
                profile: profile.profile,
                path,
            })
        }
        WorkflowCliCommand::Rm {
            profile,
            workflow_id,
        } => {
            let _ = InstanceProfile::from_label(&profile.profile)?;
            profile_command::validate_file_id(&workflow_id)?;
            if workflow_id == "goal" {
                anyhow::bail!("embedded workflow `goal` cannot be deleted");
            }
            Ok(ProfileWorkflowCommand::Delete {
                profile: profile.profile,
                workflow_id,
            })
        }
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

fn tasks_cli_to_command(args: TasksArgs) -> TasksCommand {
    match args
        .command
        .unwrap_or(TasksCliCommand::List { json: false })
    {
        TasksCliCommand::List { json } => TasksCommand::List { json },
        TasksCliCommand::Get { task_id, json } => TasksCommand::Get { task_id, json },
        TasksCliCommand::Cancel { task_id, json } => TasksCommand::Cancel { task_id, json },
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
        labels,
        include_logs: args.include_logs,
        dry_run: args.dry_run,
    })
}

fn profile_cli_to_command(command: ProfileCliCommand) -> anyhow::Result<ProfileCommand> {
    match command {
        ProfileCliCommand::Current => Ok(ProfileCommand::Current),
        ProfileCliCommand::List(args) => Ok(ProfileCommand::List {
            scope: args.scope,
            format: args.format,
        }),
        ProfileCliCommand::Find(args) => Ok(ProfileCommand::Find {
            tags: args.tag,
            intents: args.intent,
            channel: args.channel,
            format: args.format,
        }),
        ProfileCliCommand::Show(args) => Ok(ProfileCommand::Show {
            reference: args.reference,
            view: if args.manifest {
                profile_command::ProfileShowView::Manifest
            } else if args.resolved {
                profile_command::ProfileShowView::Resolved
            } else if args.sources {
                profile_command::ProfileShowView::Sources
            } else {
                profile_command::ProfileShowView::Summary
            },
            format: args.format,
        }),
        ProfileCliCommand::Check(args) => Ok(ProfileCommand::Check {
            reference: args.reference,
            strict: args.strict,
            format: args.format,
        }),
        ProfileCliCommand::Init(args) => Ok(ProfileCommand::Init {
            directory: args.directory,
            id: args.id,
            name: args.name,
            template: args.template,
            register: args.register,
            with_runtime: args.with_runtime,
        }),
        ProfileCliCommand::Register(args) => Ok(ProfileCommand::Register {
            path: args.path,
            alias: args.alias,
            replace: args.replace,
        }),
        ProfileCliCommand::Unregister { reference } => Ok(ProfileCommand::Unregister { reference }),
        ProfileCliCommand::Set(args) => Ok(ProfileCommand::Set {
            reference: args.reference,
            field: args.field,
            value: args.value,
            dry_run: args.dry_run,
        }),
        ProfileCliCommand::Unset(args) => Ok(ProfileCommand::Unset {
            reference: args.reference,
            field: args.field,
            dry_run: args.dry_run,
        }),
        ProfileCliCommand::Ask(args) => Ok(ProfileCommand::Ask {
            reference: args.reference,
            task: args.task.join(" "),
            named: args.named,
            agent_id: args.agent_id,
        }),
        ProfileCliCommand::Start(args) => Ok(ProfileCommand::Start {
            reference: args.reference,
            instance: args.instance,
        }),
        ProfileCliCommand::Stop(args) => Ok(ProfileCommand::Stop {
            reference: args.reference,
            instance: args.instance,
            force: args.force,
        }),
        ProfileCliCommand::Restart(args) => Ok(ProfileCommand::Restart {
            reference: args.reference,
            instance: args.instance,
            force: args.force,
        }),
        ProfileCliCommand::Status(args) => Ok(ProfileCommand::Status {
            reference: args.reference,
            all: args.all,
            instance: args.instance,
            format: args.format,
        }),
        ProfileCliCommand::Channel { command } => Ok(ProfileCommand::Channel(match command {
            ProfileChannelCliCommand::List { reference, format } => {
                profile_command::ProfileChannelCommand::List { reference, format }
            }
            ProfileChannelCliCommand::UpsertFeishu {
                reference,
                id,
                disabled,
                transport,
                app_id_env,
                app_secret_env,
                host,
                port,
                path,
                verification_token_env,
            } => profile_command::ProfileChannelCommand::UpsertFeishu {
                reference,
                id,
                enabled: !disabled,
                transport: match transport.as_str() {
                    "websocket" => crate::runtime_config::FeishuTransport::WebSocket,
                    "event-hook" => crate::runtime_config::FeishuTransport::EventHook,
                    _ => unreachable!("validated by clap"),
                },
                app_id_env,
                app_secret_env,
                host,
                port,
                path,
                verification_token_env,
            },
            ProfileChannelCliCommand::Enable { reference, id } => {
                profile_command::ProfileChannelCommand::Enable { reference, id }
            }
            ProfileChannelCliCommand::Disable { reference, id } => {
                profile_command::ProfileChannelCommand::Disable { reference, id }
            }
            ProfileChannelCliCommand::Remove {
                reference,
                id,
                force,
            } => profile_command::ProfileChannelCommand::Remove {
                reference,
                id,
                force,
            },
        })),
        ProfileCliCommand::Resource { command } => Ok(ProfileCommand::Resource(match command {
            ProfileResourceCliCommand::List { reference } => {
                profile_command::ProfileResourceCommand::List { reference }
            }
            ProfileResourceCliCommand::Show {
                reference,
                resource,
            } => profile_command::ProfileResourceCommand::Show {
                reference,
                resource,
            },
            ProfileResourceCliCommand::Check { reference } => {
                profile_command::ProfileResourceCommand::Check { reference }
            }
        })),
        ProfileCliCommand::Registry { command } => Ok(ProfileCommand::Registry(match command {
            ProfileRegistryCliCommand::Info => profile_command::ProfileRegistryCommand::Info,
            ProfileRegistryCliCommand::List => profile_command::ProfileRegistryCommand::List,
            ProfileRegistryCliCommand::Repair => profile_command::ProfileRegistryCommand::Repair,
            ProfileRegistryCliCommand::RebuildIndex => {
                profile_command::ProfileRegistryCommand::RebuildIndex
            }
        })),
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
        wait_background_tasks: args.wait_background_tasks,
        async_agent: false,
        permissions: args.permissions,
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
    // TUI defaults to async-agent mode; --sync opts out, --async keeps the default.
    let async_agent = if args.sync { false } else { true };
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
        wait_background_tasks: false,
        async_agent,
        permissions: None,
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
        wait_background_tasks: args.wait_background_tasks,
        async_agent: false,
        permissions: args.permissions,
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
        wait_background_tasks: args.wait_background_tasks,
        async_agent: false,
        permissions: args.permissions,
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
                validate_profile_arg(&value).map_err(anyhow::Error::msg)?;
                if profile.replace(value).is_some() {
                    anyhow::bail!("--profile may only be specified once");
                }
                i += 2;
            }
            value if value.starts_with("--profile=") => {
                let value = value.trim_start_matches("--profile=").to_string();
                validate_profile_arg(&value).map_err(anyhow::Error::msg)?;
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
