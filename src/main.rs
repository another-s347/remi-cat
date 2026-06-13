mod host_admin;
mod instance_profile;
mod profile_command;
mod runtime_config;
mod secret_store;
mod session;
mod tui_app;
mod tui_markdown;
mod web_chat;

use anyhow::Context;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bot_core::im_tools::{
    decode_agent_file_key, encode_agent_file_key, AcpBindingDeleteRequest, AcpBindingUpsertRequest,
    BoundImChannel, DownloadedImFile, ImDownloadRequest, ImFileBridge, ImUploadRequest,
    SubSessionBindingUpsertRequest, UploadedImFile,
};
use bot_core::{
    install_embedded_agent_profiles, install_embedded_model_profiles, AccountBalance, AccountUsage,
    AccountUsageStatus, AgentProfile, AgentRegistry, CatBot, CatBotBuilder, CatEvent, Content,
    ContentPart, GoalMaxRounds, ImAttachment, ImDocument, ModelProfileRegistry, SkillDocument,
    StreamOptions,
};
use futures::StreamExt;
use im_feishu::{FeishuEvent, FeishuEventHookConfig, FeishuGateway, FeishuMessage, StreamingCard};
use instance_profile::{InstanceProfile, DIAGNOSTIC_PROFILE_NAME};
use profile_command::{
    apply_runtime_config_entries, available_container_name, configured_ports,
    ensure_builtin_diagnostic_profile, first_available_port, parse_profile_command,
    prefix_short_config_entry, print_port_adjustment, run_noninteractive_setup,
    run_profile_command, ProfileCommand,
};
use remi_agentloop::types::{SubSessionEvent, SubSessionEventPayload};
use runtime_config::{
    detect_setup_state, has_legacy_env_credentials, load_dotenv_pairs, upsert_dotenv_value,
    write_runtime_config, FeishuTransport, ImMode, RuntimeConfig, RuntimeSandboxKind, SetupState,
};
use secret_store::{apply_entries_to_env, redaction_entries, SecretStore};
use session::{ChannelBinding, SessionRuntime, SubSessionKind};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex;
use tracing::{info, warn};
use user_store::UserStore;

const FEISHU_CHANNEL: &str = "feishu";
const CLI_CHANNEL: &str = "cli";
const CLI_CHAT_ID: &str = "local-dev";
const CLI_USER_ID: &str = "local-user";
const CLI_USERNAME: &str = "local-user";
const SESSION_DEBUG_METADATA_KEY: &str = "debug";
const SESSION_MODEL_PROFILE_METADATA_KEY: &str = "model_profile_id";
const SESSION_INPUT_HISTORY_METADATA_KEY: &str = "input_history";
const MAX_COMMAND_PREPROCESS_DEPTH: usize = 8;
const DEFAULT_UPDATE_REPO: &str = "another-s347/remi-cat";
const DEFAULT_UPDATE_GIT_URL: &str = "https://github.com/another-s347/remi-cat.git";
const DEFAULT_FEEDBACK_REPO: &str = "another-s347/remi-cat";
const GITHUB_API_VERSION: &str = "2026-03-10";

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppCommand {
    Run(CliConfig),
    Setup(Vec<String>),
    Doctor,
    Secrets(SecretCommand),
    ConfigSet(Vec<String>),
    SandboxSet(Vec<String>),
    Profile(ProfileCommand),
    Feishu(FeishuCommand),
    Update(UpdateCommand),
    Feedback(FeedbackCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalArgs {
    profile: Option<String>,
    command_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FeishuCommand {
    Init,
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdateCommand {
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
struct UpdateStatus {
    current_version: String,
    latest_version: String,
    latest_tag: String,
    update_available: bool,
    repo: String,
    git_url: String,
}

#[derive(Debug, serde::Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FeedbackCommand {
    title: String,
    body: String,
    repo: Option<String>,
    labels: Vec<String>,
    include_logs: bool,
    dry_run: bool,
}

#[derive(Debug, serde::Serialize)]
struct GitHubIssueCreateRequest {
    title: String,
    body: String,
    labels: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct GitHubIssueCreateResponse {
    html_url: String,
    number: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SecretCommand {
    List,
    Get(String),
    Set { key: String, value: String },
    Delete(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliConfig {
    enabled: bool,
    tui: bool,
    resume: bool,
    resume_session_id: Option<String>,
    once: Option<String>,
    pure_prompt: bool,
    admin_only: bool,
    channel_id: String,
    user_id: String,
    username: String,
}

impl CliConfig {
    fn from_args(args: &[String]) -> anyhow::Result<Self> {
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

fn parse_command(args: &[String]) -> anyhow::Result<AppCommand> {
    match args.first().map(String::as_str) {
        Some("setup") => Ok(AppCommand::Setup(args[1..].to_vec())),
        Some("doctor") | Some("check") => Ok(AppCommand::Doctor),
        Some("secrets") | Some("secret") => {
            Ok(AppCommand::Secrets(parse_secret_command(&args[1..])?))
        }
        Some("config") => parse_config_command(&args[1..]),
        Some("sandbox") => parse_sandbox_command(&args[1..]),
        Some("profile") => Ok(AppCommand::Profile(parse_profile_command(&args[1..])?)),
        Some("feishu") => Ok(AppCommand::Feishu(parse_feishu_command(&args[1..])?)),
        Some("update") => Ok(AppCommand::Update(parse_update_command(&args[1..])?)),
        Some("feedback") => Ok(AppCommand::Feedback(parse_feedback_command(&args[1..])?)),
        Some("issue") => Ok(AppCommand::Feedback(parse_issue_command(&args[1..])?)),
        _ => Ok(AppCommand::Run(CliConfig::from_args(args)?)),
    }
}

fn parse_secret_command(args: &[String]) -> anyhow::Result<SecretCommand> {
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

fn parse_config_command(args: &[String]) -> anyhow::Result<AppCommand> {
    match args.first().map(String::as_str) {
        Some("set") => Ok(AppCommand::ConfigSet(args[1..].to_vec())),
        Some(other) => anyhow::bail!("unknown `remi-cat config` subcommand `{other}`"),
        None => anyhow::bail!("usage: remi-cat config set <key=value>..."),
    }
}

fn parse_sandbox_command(args: &[String]) -> anyhow::Result<AppCommand> {
    match args.first().map(String::as_str) {
        Some("set") => Ok(AppCommand::SandboxSet(args[1..].to_vec())),
        Some(other) => anyhow::bail!("unknown `remi-cat sandbox` subcommand `{other}`"),
        None => anyhow::bail!("usage: remi-cat sandbox set <key=value>..."),
    }
}

fn parse_global_args(args: &[String]) -> anyhow::Result<GlobalArgs> {
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

fn parse_feishu_command(args: &[String]) -> anyhow::Result<FeishuCommand> {
    match args.first().map(String::as_str) {
        Some("init") => Ok(FeishuCommand::Init),
        Some("doctor") | Some("check") => Ok(FeishuCommand::Doctor),
        Some(other) => anyhow::bail!("unknown `remi-cat feishu` subcommand `{other}`"),
        None => anyhow::bail!("usage: remi-cat feishu <init|doctor>"),
    }
}

fn parse_update_command(args: &[String]) -> anyhow::Result<UpdateCommand> {
    match args.first().map(String::as_str) {
        Some("check") => {
            let mut json = false;
            for arg in &args[1..] {
                match arg.as_str() {
                    "--json" => json = true,
                    other => anyhow::bail!("unknown `remi-cat update check` option `{other}`"),
                }
            }
            Ok(UpdateCommand::Check { json })
        }
        Some("self") => {
            let mut version = None;
            let mut force = false;
            let mut dry_run = false;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--version" => {
                        if version.is_some() {
                            anyhow::bail!("--version may only be specified once");
                        }
                        version = Some(next_arg(args, i)?);
                        i += 1;
                    }
                    value if value.starts_with("--version=") => {
                        if version.is_some() {
                            anyhow::bail!("--version may only be specified once");
                        }
                        version = Some(value.trim_start_matches("--version=").to_string());
                    }
                    "--force" => force = true,
                    "--dry-run" => dry_run = true,
                    other => anyhow::bail!("unknown `remi-cat update self` option `{other}`"),
                }
                i += 1;
            }
            Ok(UpdateCommand::SelfUpdate {
                version,
                force,
                dry_run,
            })
        }
        Some(other) => anyhow::bail!("unknown `remi-cat update` subcommand `{other}`"),
        None => anyhow::bail!(
            "usage: remi-cat update <check|self>\n       remi-cat update check [--json]\n       remi-cat update self [--version <version-or-tag>] [--force] [--dry-run]"
        ),
    }
}

fn parse_issue_command(args: &[String]) -> anyhow::Result<FeedbackCommand> {
    match args.first().map(String::as_str) {
        Some("create") => parse_feedback_command(&args[1..]),
        Some(other) => anyhow::bail!("unknown `remi-cat issue` subcommand `{other}`"),
        None => anyhow::bail!("usage: remi-cat issue create --title <title> [--body <body>]"),
    }
}

fn parse_feedback_command(args: &[String]) -> anyhow::Result<FeedbackCommand> {
    let mut title = None;
    let mut body = None;
    let mut repo = None;
    let mut labels = vec!["feedback".to_string()];
    let mut include_logs = false;
    let mut dry_run = false;
    let mut positional = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--title" | "-t" => {
                title = Some(next_arg(args, i)?);
                i += 1;
            }
            value if value.starts_with("--title=") => {
                title = Some(value.trim_start_matches("--title=").trim().to_string());
            }
            "--body" | "-b" => {
                body = Some(next_arg(args, i)?);
                i += 1;
            }
            value if value.starts_with("--body=") => {
                body = Some(value.trim_start_matches("--body=").trim().to_string());
            }
            "--repo" => {
                repo = Some(next_arg(args, i)?);
                i += 1;
            }
            value if value.starts_with("--repo=") => {
                repo = Some(value.trim_start_matches("--repo=").trim().to_string());
            }
            "--label" | "--labels" => {
                labels.extend(parse_label_list(&next_arg(args, i)?));
                i += 1;
            }
            value if value.starts_with("--label=") => {
                labels.extend(parse_label_list(value.trim_start_matches("--label=")));
            }
            value if value.starts_with("--labels=") => {
                labels.extend(parse_label_list(value.trim_start_matches("--labels=")));
            }
            "--include-logs" => include_logs = true,
            "--dry-run" => dry_run = true,
            "--no-default-label" => labels.retain(|label| label != "feedback"),
            value if value.starts_with('-') => {
                anyhow::bail!("unknown `remi-cat feedback` option `{value}`");
            }
            value => positional.push(value.to_string()),
        }
        i += 1;
    }

    let positional_text = positional.join(" ").trim().to_string();
    if title.is_none() && !positional_text.is_empty() {
        title = Some(feedback_title_from_text(&positional_text));
    }
    if body.is_none() && !positional_text.is_empty() {
        body = Some(positional_text);
    }
    let title = title
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("usage: remi-cat feedback --title <title> [--body <body>]")
        })?;
    let body = body
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| title.clone());
    labels.sort();
    labels.dedup();

    Ok(FeedbackCommand {
        title,
        body,
        repo,
        labels,
        include_logs,
        dry_run,
    })
}

fn parse_label_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn feedback_title_from_text(value: &str) -> String {
    const MAX_TITLE_CHARS: usize = 80;
    let mut title = single_line(value)
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect::<String>();
    if title.chars().count() == MAX_TITLE_CHARS {
        title.push('…');
    }
    if title.is_empty() {
        "Feedback".to_string()
    } else {
        title
    }
}

fn update_repo() -> String {
    std::env::var("REMI_UPDATE_REPO")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_UPDATE_REPO.to_string())
}

fn update_git_url(repo: &str) -> String {
    std::env::var("REMI_UPDATE_GIT_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if repo == DEFAULT_UPDATE_REPO {
                DEFAULT_UPDATE_GIT_URL.to_string()
            } else {
                format!("https://github.com/{repo}.git")
            }
        })
}

fn parse_release_version(value: &str) -> anyhow::Result<semver::Version> {
    let version = value.trim().trim_start_matches('v');
    semver::Version::parse(version).with_context(|| format!("invalid release version `{value}`"))
}

fn normalize_release_tag(value: &str) -> anyhow::Result<String> {
    let version = parse_release_version(value)?;
    Ok(format!("v{version}"))
}

#[cfg(test)]
fn update_available(current: &str, latest: &str) -> anyhow::Result<bool> {
    Ok(parse_release_version(latest)? > parse_release_version(current)?)
}

fn build_cargo_install_args(git_url: &str, tag: &str) -> Vec<String> {
    vec![
        "install".to_string(),
        "--git".to_string(),
        git_url.to_string(),
        "--tag".to_string(),
        tag.to_string(),
        "remi-cat".to_string(),
        "--locked".to_string(),
        "--force".to_string(),
    ]
}

async fn fetch_latest_github_release(repo: &str) -> anyhow::Result<GitHubRelease> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let response = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "remi-cat")
        .send()
        .await
        .with_context(|| format!("failed to query {url}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub release check failed with HTTP {status}: {body}");
    }

    response
        .json::<GitHubRelease>()
        .await
        .context("failed to parse GitHub release response")
}

async fn build_update_status() -> anyhow::Result<UpdateStatus> {
    let repo = update_repo();
    let git_url = update_git_url(&repo);
    let release = fetch_latest_github_release(&repo).await?;
    let latest = parse_release_version(&release.tag_name)?;
    let current = parse_release_version(env!("CARGO_PKG_VERSION"))?;
    Ok(UpdateStatus {
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        latest_tag: release.tag_name,
        update_available: latest > current,
        repo,
        git_url,
    })
}

async fn run_update_command(command: UpdateCommand) -> anyhow::Result<()> {
    match command {
        UpdateCommand::Check { json } => {
            let status = build_update_status().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("current: {}", status.current_version);
                println!("latest: {} ({})", status.latest_version, status.latest_tag);
                println!(
                    "update_available: {}",
                    if status.update_available { "yes" } else { "no" }
                );
                if status.update_available {
                    println!("Run: remi-cat update self");
                }
            }
            Ok(())
        }
        UpdateCommand::SelfUpdate {
            version,
            force,
            dry_run,
        } => {
            let repo = update_repo();
            let git_url = update_git_url(&repo);
            let target_tag = match version {
                Some(value) => normalize_release_tag(&value)?,
                None => build_update_status().await?.latest_tag,
            };
            let target_version = parse_release_version(&target_tag)?;
            let current_version = parse_release_version(env!("CARGO_PKG_VERSION"))?;
            if target_version <= current_version && !force {
                println!(
                    "remi-cat is already at {}. Use --force to reinstall {}.",
                    current_version, target_tag
                );
                return Ok(());
            }

            let install_args = build_cargo_install_args(&git_url, &target_tag);
            if dry_run {
                println!("cargo {}", install_args.join(" "));
                return Ok(());
            }

            println!(
                "Installing remi-cat {} from {} via cargo install...",
                target_tag, git_url
            );
            let status = TokioCommand::new("cargo")
                .args(&install_args)
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .await
                .context("failed to run cargo install")?;
            if !status.success() {
                anyhow::bail!("cargo install failed with status {status}");
            }
            println!("remi-cat updated to {target_tag}.");
            println!("Restart any running remi-cat profile processes to use the new binary.");
            Ok(())
        }
    }
}

async fn run_feedback_command(
    store: Arc<Mutex<SecretStore>>,
    profile: &InstanceProfile,
    data_dir: &Path,
    command: FeedbackCommand,
) -> anyhow::Result<()> {
    let repo = feedback_repo(command.repo.as_deref())?;
    let secret_entries = store.lock().await.entries()?;
    let redactions = redaction_entries(&secret_entries);
    let body = build_feedback_issue_body(
        &command.body,
        profile,
        data_dir,
        command.include_logs,
        &redactions,
    );
    let payload = GitHubIssueCreateRequest {
        title: command.title,
        body,
        labels: command.labels,
    };

    if command.dry_run {
        println!("{}", serde_json::to_string_pretty(&payload)?);
        println!("repo: {repo}");
        return Ok(());
    }

    let Some(token) = github_issue_token(&secret_entries) else {
        let url = github_new_issue_url(&repo, &payload);
        println!("GitHub token not found; open this URL to create the issue:");
        println!("{url}");
        println!(
            "To submit directly, set GITHUB_TOKEN, GH_TOKEN, or REMI_GITHUB_TOKEN with Issues: write permission."
        );
        return Ok(());
    };

    let response = create_github_issue(&repo, &token, &payload).await?;
    println!(
        "Created GitHub issue #{}: {}",
        response.number, response.html_url
    );
    Ok(())
}

fn feedback_repo(override_repo: Option<&str>) -> anyhow::Result<String> {
    let repo = override_repo
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("REMI_FEEDBACK_REPO").ok())
        .unwrap_or_else(|| DEFAULT_FEEDBACK_REPO.to_string());
    let repo = repo.trim().trim_start_matches('/').trim_end_matches('/');
    let parts = repo.split('/').collect::<Vec<_>>();
    if parts.len() != 2 || parts.iter().any(|part| part.trim().is_empty()) {
        anyhow::bail!("invalid GitHub repo `{repo}`; expected owner/repo");
    }
    Ok(repo.to_string())
}

fn github_issue_token(secrets: &std::collections::BTreeMap<String, String>) -> Option<String> {
    ["GITHUB_TOKEN", "GH_TOKEN", "REMI_GITHUB_TOKEN"]
        .iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .or_else(|| secrets.get(*key).cloned())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn build_feedback_issue_body(
    user_body: &str,
    profile: &InstanceProfile,
    data_dir: &Path,
    include_logs: bool,
    redactions: &HashMap<String, String>,
) -> String {
    let setup_state = detect_setup_state(data_dir);
    let setup_label = match &setup_state {
        SetupState::Initialized { .. } => "initialized",
        SetupState::Invalid { .. } => "invalid",
        SetupState::LegacyEnvCompatible { .. } => "legacy-env-compatible",
        SetupState::Uninitialized { .. } => "uninitialized",
    };
    let mut body = format!(
        "{}\n\n---\n\n### Diagnostics\n\n```text\nremi-cat_version: {}\nos: {}\narch: {}\nprofile: {}\nsetup_state: {}\n{}\n{}\n```\n",
        user_body.trim(),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        profile.label(),
        setup_label,
        sdk_doctor_report(data_dir),
        sandbox_doctor_report(data_dir, &setup_state),
    );
    if include_logs {
        let logs = collect_feedback_logs(profile, redactions);
        if logs.trim().is_empty() {
            body.push_str("\n### Logs\n\nNo log files found.\n");
        } else {
            body.push_str("\n### Logs\n\n```text\n");
            body.push_str(&logs.replace("```", "'''"));
            body.push_str("\n```\n");
        }
    } else {
        body.push_str(
            "\nLogs omitted. Re-run with `--include-logs` to attach recent local logs.\n",
        );
    }
    body
}

fn collect_feedback_logs(
    profile: &InstanceProfile,
    redactions: &HashMap<String, String>,
) -> String {
    let mut sections = Vec::new();
    for path in [profile.log_file(), profile.log_dir().join("tui.log")] {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let redacted = redact_known_secrets(&text, redactions);
            sections.push(format!(
                "== {} ==\n{}",
                path.display(),
                tail_lines(&redacted, 200)
            ));
        }
    }
    sections.join("\n\n")
}

fn redact_known_secrets(text: &str, redactions: &HashMap<String, String>) -> String {
    redactions.values().fold(text.to_string(), |acc, secret| {
        if secret.is_empty() {
            acc
        } else {
            acc.replace(secret, "***REDACTED***")
        }
    })
}

fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

async fn create_github_issue(
    repo: &str,
    token: &str,
    payload: &GitHubIssueCreateRequest,
) -> anyhow::Result<GitHubIssueCreateResponse> {
    let url = format!("https://api.github.com/repos/{repo}/issues");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .header(reqwest::header::USER_AGENT, "remi-cat")
        .bearer_auth(token)
        .json(payload)
        .send()
        .await
        .with_context(|| format!("failed to create GitHub issue at {url}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("GitHub issue creation failed with HTTP {status}: {body}");
    }

    response
        .json::<GitHubIssueCreateResponse>()
        .await
        .context("failed to parse GitHub issue response")
}

fn github_new_issue_url(repo: &str, payload: &GitHubIssueCreateRequest) -> String {
    let mut url = format!(
        "https://github.com/{repo}/issues/new?title={}&body={}",
        percent_encode_query(&payload.title),
        percent_encode_query(&payload.body)
    );
    if !payload.labels.is_empty() {
        url.push_str("&labels=");
        url.push_str(&percent_encode_query(&payload.labels.join(",")));
    }
    url
}

fn percent_encode_query(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

struct LocalImFileBridge {
    gateway: Option<FeishuGateway>,
}

impl ImFileBridge for LocalImFileBridge {
    fn download<'a>(
        &'a self,
        req: ImDownloadRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<DownloadedImFile>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(gateway) = &self.gateway else {
                anyhow::bail!("current transport does not support IM file download");
            };
            if req.platform != FEISHU_CHANNEL {
                anyhow::bail!("unsupported platform: {}", req.platform);
            }
            let (mime_type, file_name, content, source_label) =
                if let Some(key) = req.attachment_key.filter(|k| !k.is_empty()) {
                    let decoded = decode_agent_file_key(&key);
                    let owner_message_id = decoded
                        .as_ref()
                        .map(|value| value.message_id.as_str())
                        .filter(|value| !value.is_empty())
                        .unwrap_or(req.message_id.as_str());
                    let real_key = decoded
                        .as_ref()
                        .map(|value| value.file_key.as_str())
                        .unwrap_or(key.as_str());
                    let (mt, fn_, c) = gateway
                        .download_file(owner_message_id, real_key, &req.file_type)
                        .await?;
                    (mt, fn_, c, format!("attachment:{real_key}"))
                } else if let Some(url) = req.document_url.filter(|u| !u.is_empty()) {
                    let (mt, fn_, c) = gateway.download_document(&url).await?;
                    (mt, fn_, c, url)
                } else {
                    anyhow::bail!("download request must specify attachment_key or document_url");
                };
            Ok(DownloadedImFile {
                file_name,
                mime_type,
                content,
                source_label,
            })
        })
    }

    fn upload<'a>(
        &'a self,
        req: ImUploadRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<UploadedImFile>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(gateway) = &self.gateway else {
                anyhow::bail!("current transport does not support IM file upload");
            };
            if req.platform != FEISHU_CHANNEL {
                anyhow::bail!("unsupported platform: {}", req.platform);
            }
            let file_key = gateway
                .upload_file(&req.file_name, &req.mime_type, &req.content, &req.file_type)
                .await?;
            let sent_message_id = if !req.message_id.is_empty() {
                match gateway
                    .reply_file(&req.message_id, &file_key, &req.file_type)
                    .await
                {
                    Ok(message_id) => message_id,
                    Err(_) => {
                        gateway
                            .send_file(&req.chat_id, &file_key, &req.file_type)
                            .await?
                    }
                }
            } else {
                gateway
                    .send_file(&req.chat_id, &file_key, &req.file_type)
                    .await?
            };
            Ok(UploadedImFile {
                file_name: req.file_name,
                file_key: file_key.clone(),
                message_id: sent_message_id.clone(),
                resource_url: gateway.file_resource_url(
                    &sent_message_id,
                    &file_key,
                    &req.file_type,
                ),
            })
        })
    }

    fn acp_binding_upsert<'a>(
        &'a self,
        _request: AcpBindingUpsertRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move { Ok(()) })
    }

    fn acp_binding_delete<'a>(
        &'a self,
        _request: AcpBindingDeleteRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move { Ok(()) })
    }

    fn sub_session_binding_upsert<'a>(
        &'a self,
        request: SubSessionBindingUpsertRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Option<BoundImChannel>>> + Send + 'a>,
    > {
        Box::pin(async move {
            if request.platform != FEISHU_CHANNEL {
                return Ok(None);
            }
            let Some(gateway) = &self.gateway else {
                return Ok(None);
            };
            let title = request
                .title
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("{} {}", request.target, request.sub_session_id));
            let chat_name = format!("remi {}: {}", request.kind, title);
            if request.kind == "fork"
                && request
                    .parent_thread_id
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
            {
                let topic_text = format!(
                    "Fork session `{}` is bound here.\nparent_session_id: {}\ntarget: {}",
                    request.sub_session_id, request.parent_session_id, request.target
                );
                let (_, thread_id) = gateway
                    .send_topic_text(&request.parent_channel_id, &topic_text)
                    .await
                    .with_context(|| {
                        format!(
                            "POST /open-apis/im/v1/messages failed while binding fork topic; parent_chat_id={}, parent_thread_id={:?}, sub_session_id={}",
                            request.parent_channel_id,
                            request.parent_thread_id,
                            request.sub_session_id,
                        )
                    })?;
                return Ok(Some(BoundImChannel {
                    platform: FEISHU_CHANNEL.to_string(),
                    channel_id: feishu_topic_channel_id(&request.parent_channel_id, &thread_id),
                }));
            }
            let chat_id = gateway
                .create_sub_session_chat(
                    &chat_name,
                    &request.parent_channel_id,
                    request.actor_user_id.as_deref(),
                )
                .await
                .with_context(|| {
                    format!(
                        "POST /open-apis/im/v1/chats?user_id_type=open_id failed while binding sub-session IM channel; owner_id/user_id_list={:?}, bot_id_list=[app_id], parent_chat_id={}, sub_session_id={}, kind={}",
                        request.actor_user_id,
                        request.parent_channel_id,
                        request.sub_session_id,
                        request.kind
                    )
                })?;
            let _ = gateway
                .send_text(
                    &chat_id,
                    &format!(
                        "Sub-session `{}` ({}) is bound here.\nparent_session_id: {}\ntarget: {}",
                        request.sub_session_id,
                        request.kind,
                        request.parent_session_id,
                        request.target
                    ),
                )
                .await;
            Ok(Some(BoundImChannel {
                platform: FEISHU_CHANNEL.to_string(),
                channel_id: chat_id,
            }))
        })
    }
}

struct Runtime {
    bot: Rc<CatBot>,
    secret_store: Arc<Mutex<SecretStore>>,
    user_store: Arc<UserStore>,
    sessions: Arc<Mutex<SessionRuntime>>,
    im_bridge: Arc<dyn ImFileBridge>,
    root_agent_id: String,
    data_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FeishuCredentialChoice {
    ReuseExisting,
    OverwriteWithLarkCli,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LarkCliConfigSnapshot {
    path: Option<PathBuf>,
    app_id: Option<String>,
    app_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandRunSummary {
    lines: Vec<String>,
    first_url: Option<String>,
    success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthStatusSummary {
    success: bool,
    output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FeishuDoctorStatus {
    lark_cli_installed: bool,
    lark_cli_config: Option<LarkCliConfigSnapshot>,
    auth_status: Option<AuthStatusSummary>,
    remi_app_id_present: bool,
    remi_app_secret_present: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let explicit_data_dir = std::env::var_os("REMI_DATA_DIR").map(PathBuf::from);
    let _ = dotenvy::dotenv();
    let secret_store = Arc::new(Mutex::new(SecretStore::from_env()));
    let startup_secrets = secret_store.lock().await.entries()?;
    apply_entries_to_env(&startup_secrets);

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let global_args = parse_global_args(&raw_args)?;
    let command = parse_command(&global_args.command_args)?;
    let selected_profile = resolve_instance_profile(global_args.profile, explicit_data_dir)?;
    if selected_profile.label() == DIAGNOSTIC_PROFILE_NAME {
        ensure_builtin_diagnostic_profile()?;
    }
    let mut data_dir = selected_profile.data_dir.clone();
    std::fs::create_dir_all(&data_dir)?;
    let _observability_guard = init_observability(
        matches!(
            &command,
            AppCommand::Run(cli) if cli.tui
        ),
        &data_dir,
    )?;

    if let AppCommand::Profile(profile_command) = &command {
        run_profile_command(profile_command)?;
        return Ok(());
    }

    match command {
        AppCommand::Setup(entries) => {
            if entries.is_empty() {
                run_setup(&selected_profile, &mut data_dir, Arc::clone(&secret_store)).await?;
            } else {
                run_noninteractive_setup(&selected_profile, &data_dir, &entries)?;
            }
            return Ok(());
        }
        AppCommand::Doctor => {
            run_doctor(&selected_profile, &data_dir)?;
            return Ok(());
        }
        AppCommand::Secrets(command) => {
            println!(
                "{}",
                run_secret_command(Arc::clone(&secret_store), &command, true).await?
            );
            return Ok(());
        }
        AppCommand::ConfigSet(entries) => {
            apply_runtime_config_entries(&selected_profile, &data_dir, &entries, false)?;
            return Ok(());
        }
        AppCommand::SandboxSet(entries) => {
            let entries = entries
                .iter()
                .map(|entry| prefix_short_config_entry("sandbox", entry))
                .collect::<Vec<_>>();
            apply_runtime_config_entries(&selected_profile, &data_dir, &entries, false)?;
            return Ok(());
        }
        AppCommand::Profile(_) => unreachable!(),
        AppCommand::Feishu(FeishuCommand::Init) => {
            run_feishu_init(Arc::clone(&secret_store)).await?;
            return Ok(());
        }
        AppCommand::Feishu(FeishuCommand::Doctor) => {
            run_feishu_doctor().await?;
            return Ok(());
        }
        AppCommand::Update(command) => {
            run_update_command(command).await?;
            return Ok(());
        }
        AppCommand::Feedback(command) => {
            run_feedback_command(
                Arc::clone(&secret_store),
                &selected_profile,
                &data_dir,
                command,
            )
            .await?;
            return Ok(());
        }
        AppCommand::Run(ref cli) => {
            unsafe {
                std::env::set_var("REMI_DATA_DIR", &data_dir);
            }
            if let SetupState::Initialized { config, .. } = detect_setup_state(&data_dir) {
                config.apply_env_defaults();
                data_dir = std::path::PathBuf::from(
                    std::env::var("REMI_DATA_DIR").unwrap_or_else(|_| config.data_dir.clone()),
                );
            }
            if !matches!(cli.admin_only, true)
                && !matches!(
                    detect_setup_state(&data_dir),
                    SetupState::Initialized { .. }
                )
                && !has_legacy_env_credentials()
            {
                anyhow::bail!(
                    "remi-cat is not initialized yet. Run `remi-cat setup` first, or provide legacy env config."
                );
            }
        }
    }

    install_embedded_model_profiles(data_dir.join("models"))?;
    install_embedded_agent_profiles(data_dir.join("agents"))?;
    std::fs::create_dir_all(data_dir.join("workflows"))?;

    let cli = match command {
        AppCommand::Run(cli) => cli,
        _ => unreachable!(),
    };
    let root_agent_id = std::env::var("REMI_AGENT_ID").unwrap_or_else(|_| "default".to_string());
    let agents_dir = std::env::var("REMI_AGENTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("agents"));

    if cli.admin_only {
        let sessions = Arc::new(Mutex::new(SessionRuntime::load(data_dir.clone())?));
        maybe_start_admin(host_admin::AdminState {
            agents_dir,
            skills_dir: data_dir.join("skills"),
            workspace_dir: current_workspace_dir(&data_dir),
            secret_store: Arc::clone(&secret_store),
            sessions,
            root_agent_id,
            setup_state: detect_setup_state(&data_dir),
            web_chat: None,
        })
        .await?;
        tokio::signal::ctrl_c().await?;
        return Ok(());
    }

    let im_disabled = matches!(im_mode_from_env(), ImMode::Disabled);
    let gateway = if im_disabled || cli.enabled || cli.tui || cli.once.is_some() {
        None
    } else {
        match (
            std::env::var("FEISHU_APP_ID").ok(),
            std::env::var("FEISHU_APP_SECRET").ok(),
        ) {
            (Some(app_id), Some(app_secret))
                if !app_id.trim().is_empty() && !app_secret.trim().is_empty() =>
            {
                Some(FeishuGateway::new(app_id, app_secret))
            }
            _ => {
                info!("Feishu credentials are absent; starting in Web-only mode");
                None
            }
        }
    };

    let bridge: Arc<dyn ImFileBridge> = Arc::new(LocalImFileBridge {
        gateway: gateway.clone(),
    });
    let bot = Rc::new(
        CatBotBuilder::from_env()?
            .im_bridge(Arc::clone(&bridge))
            .build()?,
    );
    bot.update_secret_redactor(&redaction_entries(&secret_store.lock().await.entries()?));
    let sessions = Arc::new(Mutex::new(SessionRuntime::load(data_dir.clone())?));
    let runtime = Rc::new(Runtime {
        bot,
        secret_store,
        user_store: Arc::new(UserStore::load(data_dir.join("users.json"))?),
        sessions,
        im_bridge: Arc::clone(&bridge),
        root_agent_id,
        data_dir: data_dir.clone(),
    });
    let (web_chat, web_chat_rx) = web_chat::WebChatHandle::channel();
    if !cli.pure_prompt && !cli.tui {
        maybe_start_admin(host_admin::AdminState {
            agents_dir,
            skills_dir: data_dir.join("skills"),
            workspace_dir: current_workspace_dir(&data_dir),
            secret_store: Arc::clone(&runtime.secret_store),
            sessions: Arc::clone(&runtime.sessions),
            root_agent_id: runtime.root_agent_id.clone(),
            setup_state: detect_setup_state(&data_dir),
            web_chat: Some(web_chat),
        })
        .await?;
    }

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
            tokio::task::spawn_local(web_chat::run_dispatcher(Rc::clone(&runtime), web_chat_rx));
            if cli.tui {
                return tui_app::run_tui(runtime, cli).await;
            }
            if let Some(message) = cli.once.clone() {
                if cli.pure_prompt {
                    process_prompt_message(Rc::clone(&runtime), &cli, message).await?;
                    return Ok(());
                }
                process_cli_message(Rc::clone(&runtime), &cli, message).await?;
                return Ok(());
            }
            if cli.enabled {
                return run_cli(runtime, cli).await;
            }
            match gateway {
                Some(gateway) => run_feishu(runtime, gateway).await,
                None => {
                    info!("Web Chat ready; waiting for shutdown");
                    tokio::signal::ctrl_c().await?;
                    Ok(())
                }
            }
        })
        .await
}

fn resolve_instance_profile(
    cli_profile: Option<String>,
    explicit_data_dir: Option<PathBuf>,
) -> anyhow::Result<InstanceProfile> {
    if let Some(data_dir) = explicit_data_dir {
        return Ok(InstanceProfile {
            name: None,
            data_dir,
        });
    }
    if let Some(name) = cli_profile.or_else(|| std::env::var("REMI_PROFILE").ok()) {
        return InstanceProfile::named(&name);
    }
    if let Some(data_dir) = std::env::var_os("REMI_DATA_DIR").map(PathBuf::from) {
        return Ok(InstanceProfile {
            name: None,
            data_dir,
        });
    }
    Ok(InstanceProfile::default_instance())
}

async fn maybe_start_admin(state: host_admin::AdminState) -> anyhow::Result<()> {
    let enabled = std::env::var("REMI_ADMIN_ENABLED")
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True" | "yes" | "on"))
        .unwrap_or(true);
    if !enabled {
        return Ok(());
    }

    let host = std::env::var("REMI_ADMIN_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("REMI_ADMIN_PORT").unwrap_or_else(|_| "8787".to_string());
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let local_addr = listener.local_addr()?;
    let app = host_admin::router(state);
    tokio::spawn(async move {
        info!("remi-cat admin listening on http://{local_addr}");
        if let Err(err) = axum::serve(listener, app).await {
            warn!("admin server stopped: {err:#}");
        }
    });
    Ok(())
}

fn current_workspace_dir(data_dir: &Path) -> PathBuf {
    std::env::var_os("REMI_SANDBOX_HOST_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| data_dir.to_path_buf())
}

fn init_observability(
    tui_enabled: bool,
    data_dir: &Path,
) -> anyhow::Result<Option<tracing_appender::non_blocking::WorkerGuard>> {
    let _sentry_guard = sentry::init((
        std::env::var("SENTRY_DSN").unwrap_or_default(),
        sentry::ClientOptions {
            release: sentry::release_name!(),
            ..Default::default()
        },
    ));
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "remi_cat=info,bot_core=info,im_feishu=info".into());

    if tui_enabled {
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir)?;
        let file_appender = tracing_appender::rolling::never(log_dir, "tui.log");
        let (writer, guard) = tracing_appender::non_blocking(file_appender);
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(writer)
                    .with_filter(filter),
            )
            .with(sentry::integrations::tracing::layer())
            .init();
        Ok(Some(guard))
    } else {
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_filter(filter))
            .with(sentry::integrations::tracing::layer())
            .init();
        Ok(None)
    }
}

async fn run_setup(
    profile: &InstanceProfile,
    data_dir: &mut std::path::PathBuf,
    secret_store: Arc<Mutex<SecretStore>>,
) -> anyhow::Result<()> {
    println!("remi-cat setup");
    println!("Profile: {}", profile.label());
    println!("This wizard will configure the local runtime and verify one real chat round.\n");

    if !profile.is_named() {
        let chosen_dir = prompt_with_default("Data dir", &data_dir.display().to_string())?;
        *data_dir = std::path::PathBuf::from(chosen_dir);
    } else {
        println!("Data dir: {}\n", data_dir.display());
    }
    std::fs::create_dir_all(&data_dir)?;
    let existing_config = match detect_setup_state(data_dir) {
        SetupState::Initialized {
            config_path,
            config,
        } => {
            println!(
                "Existing setup found at {}. Press Enter to keep current values or type new ones.\n",
                config_path.display()
            );
            Some(config)
        }
        SetupState::Invalid { config_path, error } => {
            println!(
                "Existing setup config at {} is invalid and will be replaced: {error}\n",
                config_path.display()
            );
            None
        }
        _ => None,
    };
    install_embedded_model_profiles(data_dir.join("models"))?;
    install_embedded_agent_profiles(data_dir.join("agents"))?;
    std::fs::create_dir_all(data_dir.join("workflows"))?;

    let agents_dir = data_dir.join("agents");
    let models_dir = data_dir.join("models");
    let agent_registry = AgentRegistry::load(&agents_dir)?;
    let model_registry = ModelProfileRegistry::load(&models_dir)?;
    let mut agents: Vec<AgentProfile> = agent_registry.profiles().cloned().collect();
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    let mut models = model_registry.list();
    models.sort_by(|a, b| a.id.cmp(&b.id));

    let root_agent_id = choose_from_list(
        "Root agent",
        &agents
            .iter()
            .map(|profile| format!("{} - {}", profile.id, profile.description))
            .collect::<Vec<_>>(),
        existing_config
            .as_ref()
            .map(|config| config.root_agent_id.as_str())
            .unwrap_or("default"),
    )?;
    let root_agent = agents
        .iter()
        .find(|profile| profile.id == root_agent_id)
        .ok_or_else(|| anyhow::anyhow!("selected root agent `{root_agent_id}` no longer exists"))?;

    let default_model_id = root_agent
        .models
        .primary
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let model_profile = choose_from_list(
        "Primary model profile",
        &models
            .iter()
            .map(|profile| format!("{} - {}", profile.id, profile.name))
            .collect::<Vec<_>>(),
        existing_config
            .as_ref()
            .map(|config| config.model_profile.as_str())
            .unwrap_or(&default_model_id),
    )?;
    let default_sandbox_kind = existing_config
        .as_ref()
        .map(|config| config.sandbox.kind.as_env_value())
        .unwrap_or("no_sandbox");
    let sandbox_kind = choose_from_list(
        "Sandbox",
        &[
            "disabled - fs only; bash tool is not exposed".to_string(),
            "no_sandbox - local fs and local bash in the configured data dir".to_string(),
            "docker - fs in a host dir mounted into a persistent Docker container".to_string(),
        ],
        default_sandbox_kind,
    )?;
    let default_feishu_transport = existing_config
        .as_ref()
        .map(|config| config.im.transport.as_env_value())
        .unwrap_or("websocket");
    let feishu_transport = choose_from_list(
        "Feishu inbound transport",
        &[
            "websocket - Feishu long connection; no public callback URL needed".to_string(),
            "event_hook - HTTP callback endpoint; requires a public URL/proxy in Feishu app settings".to_string(),
        ],
        default_feishu_transport,
    )?;

    let current_api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("REMI_API_KEY"))
        .ok();
    let api_key = prompt_secret(current_api_key.as_deref())?;
    if !api_key.trim().is_empty() {
        unsafe {
            std::env::set_var("OPENAI_API_KEY", api_key.trim());
        }
    }

    let previous_config = existing_config.clone();
    let mut config = existing_config.unwrap_or_else(|| RuntimeConfig::default_for(data_dir));
    config.data_dir = data_dir.display().to_string();
    config.root_agent_id = root_agent_id.clone();
    config.model_profile = model_profile.clone();
    if profile.is_named() && previous_config.is_none() {
        config.sandbox.container_name = format!("remi-cat-sandbox-{}", profile.label());
    }
    match sandbox_kind.as_str() {
        "docker" => {
            config.sandbox.kind = RuntimeSandboxKind::Docker;
            let default_host_dir = config.sandbox.host_dir_or_data_dir(&config.data_dir);
            config.sandbox.host_dir = prompt_with_default("Sandbox host dir", &default_host_dir)?;
            config.sandbox.container_dir =
                prompt_with_default("Sandbox container dir", &config.sandbox.container_dir)?;
            config.sandbox.image = prompt_with_default("Sandbox image", &config.sandbox.image)?;
            config.sandbox.container_name =
                prompt_with_default("Sandbox container name", &config.sandbox.container_name)?;
            config.sandbox.container_name =
                available_container_name(&config.sandbox.container_name, data_dir)?;
        }
        _ => {
            config.sandbox.kind = if sandbox_kind == "disabled" {
                RuntimeSandboxKind::Disabled
            } else {
                RuntimeSandboxKind::NoSandbox
            };
            config.sandbox.host_dir = data_dir.display().to_string();
        }
    }
    config.admin.enabled = prompt_bool_with_default("Admin enabled", config.admin.enabled)?;
    if config.admin.enabled {
        config.admin.host = prompt_with_default("Admin listen host", &config.admin.host)?;
        config.admin.port =
            prompt_with_default("Admin listen port", &config.admin.port.to_string())?
                .parse()
                .context("invalid Admin listen port")?;
    }
    config.im.transport = match feishu_transport.as_str() {
        "event_hook" | "event-hook" | "hook" => FeishuTransport::EventHook,
        _ => FeishuTransport::WebSocket,
    };
    config.im.mode = ImMode::Feishu;
    if matches!(config.im.transport, FeishuTransport::EventHook) {
        config.im.event_hook.host =
            prompt_with_default("Feishu Event Hook listen host", &config.im.event_hook.host)?;
        config.im.event_hook.port = prompt_with_default(
            "Feishu Event Hook listen port",
            &config.im.event_hook.port.to_string(),
        )?
        .parse()
        .context("invalid Feishu Event Hook listen port")?;
        config.im.event_hook.path =
            prompt_with_default("Feishu Event Hook path", &config.im.event_hook.path)?;
        config.im.event_hook.verification_token = prompt_with_default(
            "Feishu Event Hook verification token (blank to disable local token check)",
            &config.im.event_hook.verification_token,
        )?;
    }

    let mut reserved_ports = configured_ports(data_dir)?;
    if config.admin.enabled {
        let requested = config.admin.port;
        config.admin.port = first_available_port(&config.admin.host, requested, &reserved_ports)?;
        print_port_adjustment("Admin", requested, config.admin.port);
        reserved_ports.insert(config.admin.port);
    }
    if matches!(config.im.transport, FeishuTransport::EventHook) {
        let requested = config.im.event_hook.port;
        config.im.event_hook.port =
            first_available_port(&config.im.event_hook.host, requested, &reserved_ports)?;
        print_port_adjustment("Feishu Event Hook", requested, config.im.event_hook.port);
    }

    let runtime_path = write_runtime_config(data_dir, &config)?;
    match run_setup_smoke(data_dir, &config, api_key.trim()).await {
        Ok(reply) => {
            let env_path = std::path::PathBuf::from(".env");
            if !profile.is_named() {
                upsert_dotenv_value(&env_path, "REMI_DATA_DIR", &config.data_dir)?;
            }
            if !api_key.trim().is_empty() {
                secret_store
                    .lock()
                    .await
                    .set("OPENAI_API_KEY", api_key.trim())?;
            }
            println!("\nSetup verification succeeded.");
            println!("Smoke reply: {}", reply.trim());
            println!("Saved runtime config to {}", runtime_path.display());
            println!("\nNext steps:");
            println!(
                "- Local chat: remi-cat{} cli --channel support --user alice --name Alice \"Hello\"",
                profile
                    .name
                    .as_ref()
                    .map(|name| format!(" --profile {name}"))
                    .unwrap_or_default()
            );
            println!("- Feishu: run `remi-cat feishu init`, then run remi-cat");
            println!("- ACP: configure ACP settings later when needed");
            println!("- Shell: enable shell.mode only when you want local bash tools");
            Ok(())
        }
        Err(err) => {
            if let Some(previous_config) = previous_config {
                let _ = write_runtime_config(data_dir, &previous_config);
            } else {
                let _ = std::fs::remove_file(&runtime_path);
            }
            anyhow::bail!("setup verification failed: {err:#}")
        }
    }
}

fn run_doctor(profile: &InstanceProfile, data_dir: &Path) -> anyhow::Result<()> {
    let setup_state = detect_setup_state(data_dir);
    println!("remi-cat doctor");
    println!("profile: {}", profile.label());
    println!("data_dir: {}", data_dir.display());
    println!("runtime_config: {}", setup_state.config_path().display());
    match &setup_state {
        SetupState::Initialized { config, .. } => {
            println!("setup: initialized");
            println!("root_agent_id: {}", config.root_agent_id);
            println!("model_profile: {}", config.model_profile);
            println!(
                "admin: {}",
                if config.admin.enabled {
                    format!("http://{}:{}", config.admin.host, config.admin.port)
                } else {
                    "disabled".to_string()
                }
            );
            println!("sandbox_container: {}", config.sandbox.container_name);
            println!("feishu_transport: {}", config.im.transport.as_env_value());
            println!("im_mode: {}", config.im.mode.as_env_value());
            if matches!(config.im.transport, FeishuTransport::EventHook) {
                println!(
                    "feishu_event_hook: http://{}:{}{}",
                    config.im.event_hook.host, config.im.event_hook.port, config.im.event_hook.path
                );
            }
        }
        SetupState::LegacyEnvCompatible { .. } => {
            println!("setup: legacy-env-compatible (no runtime.yaml)");
        }
        SetupState::Uninitialized { .. } => {
            println!("setup: not initialized");
        }
        SetupState::Invalid { error, .. } => {
            println!("setup: invalid");
            println!("error: {error}");
        }
    }

    let api_key_present = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("REMI_API_KEY"))
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    println!(
        "api_key: {}",
        if api_key_present {
            "present"
        } else {
            "missing"
        }
    );

    let agents_dir = std::env::var("REMI_AGENTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("agents"));
    let models_dir = data_dir.join("models");
    let agent_registry = AgentRegistry::load(&agents_dir)?;
    let model_registry = ModelProfileRegistry::load(&models_dir)?;
    println!("{}", sdk_doctor_report(data_dir));
    println!("{}", sandbox_doctor_report(data_dir, &setup_state));

    if let SetupState::Initialized { config, .. } = &setup_state {
        println!(
            "root_agent_exists: {}",
            if agent_registry.get(&config.root_agent_id).is_some() {
                "yes"
            } else {
                "no"
            }
        );
        println!(
            "model_profile_exists: {}",
            if model_registry.get(&config.model_profile).is_some() {
                "yes"
            } else {
                "no"
            }
        );
        if let Some(agent) = agent_registry.get(&config.root_agent_id) {
            println!(
                "helper_model_profile: {}",
                agent.models.helper.as_deref().unwrap_or("reserved/unset")
            );
            println!(
                "vision_model_profile: {}",
                agent.models.vision.as_deref().unwrap_or("reserved/unset")
            );
            println!(
                "tool_todo: {}",
                if has_all_tools(
                    &agent.tools,
                    &[
                        "todo__add",
                        "todo__list",
                        "todo__complete",
                        "todo__update",
                        "todo__remove",
                    ],
                ) {
                    "enabled"
                } else {
                    "missing"
                }
            );
            println!(
                "tool_trigger: {}",
                if has_all_tools(
                    &agent.tools,
                    &["trigger__upsert", "trigger__list", "trigger__delete"],
                ) {
                    "enabled"
                } else {
                    "missing"
                }
            );
        }
    }
    Ok(())
}

fn has_all_tools(tools: &[String], required: &[&str]) -> bool {
    required
        .iter()
        .all(|required| tools.iter().any(|tool| tool == required))
}

fn sandbox_doctor_report(data_dir: &Path, setup_state: &SetupState) -> String {
    let config = match setup_state {
        SetupState::Initialized { config, .. } => config.clone(),
        _ => {
            let mut config = RuntimeConfig::default_for(data_dir);
            config.sandbox.kind = match std::env::var("REMI_SANDBOX_KIND")
                .ok()
                .map(|value| value.trim().to_ascii_lowercase())
                .as_deref()
            {
                Some("docker") => RuntimeSandboxKind::Docker,
                Some("no_sandbox") | Some("no-sandbox") | Some("local") => {
                    RuntimeSandboxKind::NoSandbox
                }
                Some("disabled") => RuntimeSandboxKind::Disabled,
                _ => match std::env::var("REMI_SHELL_MODE")
                    .or_else(|_| std::env::var("REMI_BASH_MODE"))
                    .ok()
                    .as_deref()
                {
                    Some("local") => RuntimeSandboxKind::NoSandbox,
                    _ => RuntimeSandboxKind::Disabled,
                },
            };
            config.sandbox.host_dir = std::env::var("REMI_SANDBOX_HOST_DIR")
                .unwrap_or_else(|_| data_dir.display().to_string());
            config.sandbox.container_dir =
                std::env::var("REMI_SANDBOX_CONTAINER_DIR").unwrap_or(config.sandbox.container_dir);
            config.sandbox.image =
                std::env::var("REMI_SANDBOX_IMAGE").unwrap_or(config.sandbox.image);
            config.sandbox.container_name = std::env::var("REMI_SANDBOX_CONTAINER_NAME")
                .unwrap_or(config.sandbox.container_name);
            config
        }
    };

    let mut lines = vec![
        "sandbox: enabled".to_string(),
        format!("sandbox_kind: {}", config.sandbox.kind.as_env_value()),
        format!(
            "sandbox_host_dir: {}",
            config.sandbox.host_dir_or_data_dir(&config.data_dir)
        ),
        format!("sandbox_container_dir: {}", config.sandbox.container_dir),
        format!(
            "sandbox_bash: {}",
            if matches!(config.sandbox.kind, RuntimeSandboxKind::Disabled) {
                "disabled"
            } else {
                "enabled"
            }
        ),
    ];

    match check_sandbox(&config) {
        Ok(report) => {
            lines.push("sandbox_status: ok".to_string());
            if !report.note.is_empty() {
                lines.push(format!("sandbox_note: {}", report.note));
            }
            if !matches!(config.sandbox.kind, RuntimeSandboxKind::Disabled) {
                lines.extend(report.command_check.lines());
            }
        }
        Err(err) => {
            lines.push("sandbox_status: error".to_string());
            lines.push(format!("sandbox_error: {err}"));
        }
    }

    lines.join("\n")
}

struct SandboxDoctorCheck {
    note: String,
    command_check: CommandCheckReport,
}

#[derive(Default)]
struct CommandCheckReport {
    required_present: Vec<String>,
    required_missing: Vec<String>,
    recommended_present: Vec<String>,
    recommended_missing: Vec<String>,
}

impl CommandCheckReport {
    fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!(
                "sandbox_commands_required: {}",
                format_command_status(&self.required_present, &self.required_missing)
            ),
            format!(
                "sandbox_commands_recommended: {}",
                format_command_status(&self.recommended_present, &self.recommended_missing)
            ),
        ];
        if !self.required_missing.is_empty() {
            lines.push(format!(
                "sandbox_commands_required_missing: {}",
                self.required_missing.join(",")
            ));
        }
        if !self.recommended_missing.is_empty() {
            lines.push(format!(
                "sandbox_commands_recommended_missing: {}",
                self.recommended_missing.join(",")
            ));
        }
        lines
    }
}

fn format_command_status(present: &[String], missing: &[String]) -> String {
    if missing.is_empty() {
        format!("ok ({})", present.join(","))
    } else {
        format!(
            "missing {} of {}",
            missing.len(),
            present.len() + missing.len()
        )
    }
}

const REQUIRED_SANDBOX_COMMANDS: &[&str] = &["bash", "sleep", "cat"];
const RECOMMENDED_SANDBOX_COMMANDS: &[&str] = &[
    "git", "curl", "wget", "rg", "python3", "pip3", "jq", "gcc", "make", "tar", "gzip", "unzip",
    "zip", "sed", "grep", "awk", "find", "xargs", "wc", "head", "tail", "sort",
];

fn check_sandbox(config: &RuntimeConfig) -> anyhow::Result<SandboxDoctorCheck> {
    let host_dir = PathBuf::from(config.sandbox.host_dir_or_data_dir(&config.data_dir));
    std::fs::create_dir_all(&host_dir)?;
    let probe = format!("remi_sandbox_doctor_{}.txt", uuid::Uuid::new_v4());
    let probe_path = host_dir.join(&probe);
    std::fs::write(&probe_path, b"remi-sandbox-ok")?;
    let read_back = std::fs::read_to_string(&probe_path)?;
    if read_back != "remi-sandbox-ok" {
        anyhow::bail!("fs probe returned unexpected content");
    }

    let result = match config.sandbox.kind {
        RuntimeSandboxKind::Disabled => Ok(SandboxDoctorCheck {
            note: "bash disabled; fs probe passed".to_string(),
            command_check: CommandCheckReport::default(),
        }),
        RuntimeSandboxKind::NoSandbox => {
            let output = std::process::Command::new("bash")
                .arg("-c")
                .arg(format!("cat {}", shell_quote(&probe)))
                .current_dir(&host_dir)
                .output()?;
            if !output.status.success() {
                anyhow::bail!(
                    "local bash probe failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            let text = String::from_utf8_lossy(&output.stdout);
            if text != "remi-sandbox-ok" {
                anyhow::bail!("local bash probe saw different file content");
            }
            let command_check = check_local_sandbox_commands(&host_dir)?;
            if !command_check.required_missing.is_empty() {
                anyhow::bail!(
                    "local sandbox missing required commands: {}",
                    command_check.required_missing.join(",")
                );
            }
            Ok(SandboxDoctorCheck {
                note: "fs and local bash see the same path".to_string(),
                command_check,
            })
        }
        RuntimeSandboxKind::Docker => {
            ensure_doctor_docker_sandbox(config, &host_dir)?;
            let output = std::process::Command::new("docker")
                .args([
                    "exec",
                    "-w",
                    &config.sandbox.container_dir,
                    &config.sandbox.container_name,
                    "bash",
                    "-lc",
                    &format!("cat {}", shell_quote(&probe)),
                ])
                .output()?;
            if !output.status.success() {
                anyhow::bail!(
                    "docker bash probe failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            let text = String::from_utf8_lossy(&output.stdout);
            if text != "remi-sandbox-ok" {
                anyhow::bail!("docker bash probe saw different file content");
            }
            let command_check = check_docker_sandbox_commands(config)?;
            if !command_check.required_missing.is_empty() {
                anyhow::bail!(
                    "docker sandbox missing required commands: {}",
                    command_check.required_missing.join(",")
                );
            }
            Ok(SandboxDoctorCheck {
                note: format!(
                    "container `{}` sees mounted fs path",
                    config.sandbox.container_name
                ),
                command_check,
            })
        }
    };

    let _ = std::fs::remove_file(probe_path);
    result
}

fn check_local_sandbox_commands(host_dir: &Path) -> anyhow::Result<CommandCheckReport> {
    let output = std::process::Command::new("bash")
        .arg("-lc")
        .arg(command_check_script())
        .current_dir(host_dir)
        .output()?;
    parse_command_check_output(output)
}

fn check_docker_sandbox_commands(config: &RuntimeConfig) -> anyhow::Result<CommandCheckReport> {
    let output = std::process::Command::new("docker")
        .args([
            "exec",
            "-w",
            &config.sandbox.container_dir,
            &config.sandbox.container_name,
            "bash",
            "-lc",
            &command_check_script(),
        ])
        .output()?;
    parse_command_check_output(output)
}

fn command_check_script() -> String {
    let required = REQUIRED_SANDBOX_COMMANDS.join(" ");
    let recommended = RECOMMENDED_SANDBOX_COMMANDS.join(" ");
    format!(
        r#"for c in {required}; do
  if command -v "$c" >/dev/null 2>&1; then printf 'required present %s\n' "$c"; else printf 'required missing %s\n' "$c"; fi
done
for c in {recommended}; do
  if command -v "$c" >/dev/null 2>&1; then printf 'recommended present %s\n' "$c"; else printf 'recommended missing %s\n' "$c"; fi
done"#
    )
}

fn parse_command_check_output(output: std::process::Output) -> anyhow::Result<CommandCheckReport> {
    if !output.status.success() {
        anyhow::bail!(
            "command probe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut report = CommandCheckReport::default();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split_whitespace();
        let Some(group) = parts.next() else { continue };
        let Some(status) = parts.next() else { continue };
        let Some(command) = parts.next() else {
            continue;
        };
        match (group, status) {
            ("required", "present") => report.required_present.push(command.to_string()),
            ("required", "missing") => report.required_missing.push(command.to_string()),
            ("recommended", "present") => report.recommended_present.push(command.to_string()),
            ("recommended", "missing") => report.recommended_missing.push(command.to_string()),
            _ => {}
        }
    }
    Ok(report)
}

fn ensure_doctor_docker_sandbox(config: &RuntimeConfig, host_dir: &Path) -> anyhow::Result<()> {
    let docker = std::process::Command::new("docker")
        .arg("--version")
        .output()
        .map_err(|err| anyhow::anyhow!("docker CLI unavailable: {err}"))?;
    if !docker.status.success() {
        anyhow::bail!("docker CLI unavailable");
    }

    let inspect = std::process::Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{.State.Running}}",
            &config.sandbox.container_name,
        ])
        .output();
    if let Ok(output) = inspect {
        if output.status.success() {
            if String::from_utf8_lossy(&output.stdout).trim() == "true" {
                return Ok(());
            }
            let start = std::process::Command::new("docker")
                .args(["start", &config.sandbox.container_name])
                .output()?;
            if start.status.success() {
                return Ok(());
            }
            anyhow::bail!(
                "docker start failed: {}",
                String::from_utf8_lossy(&start.stderr).trim()
            );
        }
    }

    let host_dir = std::fs::canonicalize(host_dir)?;
    let mount = format!("{}:{}", host_dir.display(), config.sandbox.container_dir);
    let run = std::process::Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            &config.sandbox.container_name,
            "-v",
            &mount,
            "-w",
            &config.sandbox.container_dir,
            &config.sandbox.image,
            "sleep",
            "infinity",
        ])
        .output()?;
    if run.status.success() {
        return Ok(());
    }
    anyhow::bail!(
        "docker run failed: {}",
        String::from_utf8_lossy(&run.stderr).trim()
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn sdk_doctor_report(data_dir: &Path) -> String {
    let remote_ready = env_var_present("REMI_APP_KEY") && env_var_present("REMI_PUBLIC_GRPC_ADDR");
    let partial_remote = env_var_present("REMI_APP_KEY") ^ env_var_present("REMI_PUBLIC_GRPC_ADDR");
    let mode = if remote_ready {
        "local+remote-sync"
    } else {
        "local-only"
    };
    let mut lines = vec![
        "remi_sdk: enabled".to_string(),
        format!("remi_sdk_mode: {mode}"),
        format!(
            "remi_sdk_remote_config: {}",
            if remote_ready {
                "complete"
            } else if partial_remote {
                "partial"
            } else {
                "missing"
            }
        ),
        format!(
            "remi_sdk_user_data: {}",
            data_dir.join("sdk").join("<sender_user_id>").display()
        ),
    ];
    if !remote_ready {
        lines.push(
            "remi_sdk_note: local SDK todo/trigger works; remote sync is disabled until REMI_APP_KEY and REMI_PUBLIC_GRPC_ADDR are both set"
                .to_string(),
        );
    }
    lines.join("\n")
}

fn command_doctor_report(runtime: &Runtime) -> String {
    let tools = runtime
        .bot
        .tool_list()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    let todo_enabled = has_all_tools(
        &tools,
        &[
            "todo__add",
            "todo__list",
            "todo__complete",
            "todo__update",
            "todo__remove",
        ],
    );
    let trigger_enabled = has_all_tools(
        &tools,
        &["trigger__upsert", "trigger__list", "trigger__delete"],
    );
    format!(
        "**remi-cat doctor**\n\n```text\n{}\nroot_agent_id: {}\ntool_todo: {}\ntool_trigger: {}\n```",
        sdk_doctor_report(&runtime.data_dir),
        runtime.root_agent_id,
        if todo_enabled { "enabled" } else { "missing" },
        if trigger_enabled { "enabled" } else { "missing" },
    )
}

async fn run_secret_command(
    store: Arc<Mutex<SecretStore>>,
    command: &SecretCommand,
    redact_values: bool,
) -> anyhow::Result<String> {
    let store = store.lock().await;
    match command {
        SecretCommand::List => {
            let keys = store.keys()?;
            if keys.is_empty() {
                Ok(format!("secret store `{}` is empty", store.backend_label()))
            } else {
                Ok(format!(
                    "secret store `{}` keys:\n{}",
                    store.backend_label(),
                    keys.into_iter()
                        .map(|key| format!("- `{key}`"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ))
            }
        }
        SecretCommand::Get(key) => match store.get(key)? {
            Some(value) if redact_values => Ok(format!("`{key}` is set: {}", mask_secret(&value))),
            Some(value) => Ok(format!("{key}={value}")),
            None => Ok(format!("`{key}` is not set")),
        },
        SecretCommand::Set { key, value } => {
            store.set(key, value)?;
            unsafe {
                std::env::set_var(key, value);
            }
            Ok(format!(
                "secret `{key}` saved to `{}`",
                store.backend_label()
            ))
        }
        SecretCommand::Delete(key) => {
            store.delete(key)?;
            unsafe {
                std::env::remove_var(key);
            }
            Ok(format!(
                "secret `{key}` deleted from `{}`",
                store.backend_label()
            ))
        }
    }
}

async fn handle_runtime_secret_command(
    runtime: &Runtime,
    command: &SecretCommand,
) -> anyhow::Result<String> {
    let reply = run_secret_command(Arc::clone(&runtime.secret_store), command, true).await?;
    let entries = runtime.secret_store.lock().await.entries()?;
    runtime
        .bot
        .update_secret_redactor(&redaction_entries(&entries));
    Ok(reply)
}

fn mask_secret(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= 8 {
        return "********".to_string();
    }
    format!(
        "{}…{}",
        chars.iter().take(4).collect::<String>(),
        chars
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>()
    )
}

async fn run_feishu_init(secret_store: Arc<Mutex<SecretStore>>) -> anyhow::Result<()> {
    let bin = lark_cli_bin();
    if !lark_cli_installed(&bin) {
        anyhow::bail!(
            "lark-cli is not installed or not on PATH.\nInstall it first with:\n  npx @larksuite/cli@latest install"
        );
    }

    println!("remi-cat feishu init");
    println!("This wizard will automate Feishu app setup, login, verification, and .env import.\n");

    let env_app_id = std::env::var("FEISHU_APP_ID")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let env_app_secret = std::env::var("FEISHU_APP_SECRET")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let credential_choice = match (env_app_id.as_ref(), env_app_secret.as_ref()) {
        (Some(app_id), Some(_)) => prompt_feishu_credential_choice(app_id)?,
        _ => FeishuCredentialChoice::OverwriteWithLarkCli,
    };

    if credential_choice == FeishuCredentialChoice::OverwriteWithLarkCli {
        println!("\nLaunching `lark-cli config init --new`...");
        let config_result = run_streaming_command(
            &bin,
            &["config", "init", "--new"],
            "Please open the following URL in your browser to finish Feishu app setup.",
        )
        .await?;
        if !config_result.success {
            anyhow::bail!("lark-cli config init failed. Re-run `remi-cat feishu init` after fixing the issue.");
        }
    } else {
        println!("\nReusing the current FEISHU_APP_ID / FEISHU_APP_SECRET from the environment.");
        let app_id = env_app_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("FEISHU_APP_ID must be set to reuse credentials"))?;
        let app_secret = env_app_secret
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("FEISHU_APP_SECRET must be set to reuse credentials"))?;
        ensure_lark_cli_config_for_existing_credentials(&bin, app_id, app_secret).await?;
    }

    println!("\nLaunching `lark-cli auth login --recommend`...");
    let login_result = run_streaming_command(
        &bin,
        &["auth", "login", "--recommend"],
        "Please open the following URL in your browser to finish Feishu login authorization.",
    )
    .await?;
    if !login_result.success {
        anyhow::bail!(
            "lark-cli auth login failed after app setup. The app may already exist, but login is incomplete. Re-run `remi-cat feishu init`."
        );
    }

    let auth_status = fetch_auth_status(&bin).await?;
    if !auth_status.success {
        anyhow::bail!(
            "lark-cli auth status did not report success.\n{}",
            auth_status.output.trim()
        );
    }

    if credential_choice == FeishuCredentialChoice::ReuseExisting {
        {
            let store = secret_store.lock().await;
            store.set(
                "FEISHU_APP_ID",
                env_app_id.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("FEISHU_APP_ID must be set to reuse credentials")
                })?,
            )?;
            store.set(
                "FEISHU_APP_SECRET",
                env_app_secret.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("FEISHU_APP_SECRET must be set to reuse credentials")
                })?,
            )?;
        }
        println!("\nFeishu login verification succeeded.");
        println!("Existing FEISHU_APP_ID / FEISHU_APP_SECRET were synced to the secret store.");
        println!("Next step: run `remi-cat` to start the Feishu gateway.");
        return Ok(());
    }

    let snapshot = load_lark_cli_config_snapshot()?.ok_or_else(|| {
        anyhow::anyhow!("lark-cli setup completed, but no readable Lark config file was found.")
    })?;
    let app_id = snapshot
        .app_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "lark-cli setup completed, but remi-cat could not find the Feishu app id in {}.",
                snapshot
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "the detected config".to_string())
            )
        })?;
    let app_secret = snapshot
        .app_secret
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "lark-cli setup completed, but remi-cat could not import the Feishu app secret automatically.\n\
You can still finish manually by writing FEISHU_APP_ID / FEISHU_APP_SECRET into `.env`, then rerun `remi-cat feishu doctor`."
            )
        })?;

    {
        let store = secret_store.lock().await;
        store.set("FEISHU_APP_ID", app_id)?;
        store.set("FEISHU_APP_SECRET", app_secret)?;
    }
    unsafe {
        std::env::set_var("FEISHU_APP_ID", app_id);
        std::env::set_var("FEISHU_APP_SECRET", app_secret);
    }

    println!("\nFeishu setup completed successfully.");
    if let Some(path) = snapshot.path {
        println!("Imported app credentials from {}", path.display());
    }
    println!("Updated secret store with FEISHU_APP_ID / FEISHU_APP_SECRET.");
    println!("Next step: run `remi-cat` to start the Feishu gateway.");
    Ok(())
}

async fn ensure_lark_cli_config_for_existing_credentials(
    bin: &str,
    app_id: &str,
    app_secret: &str,
) -> anyhow::Result<()> {
    let current = load_lark_cli_config_snapshot()?;
    let config_matches = current
        .as_ref()
        .map(|snapshot| {
            snapshot.app_id.as_deref() == Some(app_id)
                && snapshot
                    .app_secret
                    .as_deref()
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false)
        })
        .unwrap_or(false);

    if config_matches {
        println!("lark-cli config already matches the current Feishu app.");
        return Ok(());
    }

    println!("Initializing lark-cli config from existing Feishu app credentials...");
    let secret_input = format!("{app_secret}\n");
    let result = run_streaming_command_with_stdin(
        bin,
        &[
            "config",
            "init",
            "--app-id",
            app_id,
            "--app-secret-stdin",
            "--brand",
            "feishu",
        ],
        "Please open the following URL in your browser to finish Feishu app setup.",
        Some(&secret_input),
    )
    .await?;
    if !result.success {
        anyhow::bail!(
            "lark-cli config init failed while importing the existing Feishu credentials."
        );
    }
    Ok(())
}

async fn run_feishu_doctor() -> anyhow::Result<()> {
    let status = collect_feishu_doctor_status().await?;
    println!("remi-cat feishu doctor");
    println!(
        "lark_cli: {}",
        if status.lark_cli_installed {
            "installed"
        } else {
            "missing"
        }
    );
    if let Some(snapshot) = &status.lark_cli_config {
        println!(
            "lark_config_path: {}",
            snapshot
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_string())
        );
        println!(
            "lark_app_id: {}",
            snapshot.app_id.as_deref().unwrap_or("missing")
        );
        println!(
            "lark_app_secret_importable: {}",
            if snapshot
                .app_secret
                .as_deref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
            {
                "yes"
            } else {
                "no"
            }
        );
    } else {
        println!("lark_config_path: missing");
    }

    match &status.auth_status {
        Some(auth) => {
            println!(
                "auth_status: {}",
                if auth.success { "ok" } else { "failed" }
            );
            if !auth.output.trim().is_empty() {
                println!("auth_output: {}", auth.output.trim().replace('\n', " | "));
            }
        }
        None => println!("auth_status: unavailable"),
    }

    println!(
        "remi_feishu_app_id: {}",
        if status.remi_app_id_present {
            "present"
        } else {
            "missing"
        }
    );
    println!(
        "remi_feishu_app_secret: {}",
        if status.remi_app_secret_present {
            "present"
        } else {
            "missing"
        }
    );

    match feishu_doctor_message(&status) {
        Some(message) => println!("diagnosis: {message}"),
        None => {
            println!("diagnosis: Feishu CLI config, auth, and remi-cat credentials all look ready.")
        }
    }
    Ok(())
}

async fn collect_feishu_doctor_status() -> anyhow::Result<FeishuDoctorStatus> {
    let bin = lark_cli_bin();
    let lark_cli_installed = lark_cli_installed(&bin);
    let lark_cli_config = if lark_cli_installed {
        load_lark_cli_config_snapshot()?
    } else {
        None
    };
    let auth_status = if lark_cli_installed {
        Some(fetch_auth_status(&bin).await?)
    } else {
        None
    };
    let dotenv_pairs = load_dotenv_pairs(Path::new(".env")).unwrap_or_default();
    let remi_app_id_present = std::env::var("FEISHU_APP_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| dotenv_pairs.get("FEISHU_APP_ID").cloned())
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let remi_app_secret_present = std::env::var("FEISHU_APP_SECRET")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| dotenv_pairs.get("FEISHU_APP_SECRET").cloned())
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);

    Ok(FeishuDoctorStatus {
        lark_cli_installed,
        lark_cli_config,
        auth_status,
        remi_app_id_present,
        remi_app_secret_present,
    })
}

fn feishu_doctor_message(status: &FeishuDoctorStatus) -> Option<&'static str> {
    if !status.lark_cli_installed {
        return Some("Install lark-cli first with `npx @larksuite/cli@latest install`.");
    }
    if status
        .auth_status
        .as_ref()
        .map(|auth| auth.success)
        .unwrap_or(false)
        && (!status.remi_app_id_present || !status.remi_app_secret_present)
    {
        return Some("lark-cli is logged in, but remi-cat is still missing FEISHU_APP_ID or FEISHU_APP_SECRET.");
    }
    if !status
        .auth_status
        .as_ref()
        .map(|auth| auth.success)
        .unwrap_or(false)
        && status.remi_app_id_present
        && status.remi_app_secret_present
    {
        return Some("remi-cat already has FEISHU_APP_ID / FEISHU_APP_SECRET, but lark-cli login is missing or invalid.");
    }
    if status.lark_cli_config.is_none() {
        return Some("lark-cli is installed, but no readable app configuration was found yet. Run `remi-cat feishu init`.");
    }
    if status
        .lark_cli_config
        .as_ref()
        .and_then(|snapshot| snapshot.app_secret.as_ref())
        .is_none()
        && (!status.remi_app_id_present || !status.remi_app_secret_present)
    {
        return Some("lark-cli app config was found, but remi-cat could not import the app secret automatically. Manual .env entry may still be required.");
    }
    None
}

fn prompt_feishu_credential_choice(app_id: &str) -> anyhow::Result<FeishuCredentialChoice> {
    loop {
        print!(
            "Existing FEISHU credentials detected for `{app_id}`. Reuse them or overwrite via lark-cli? [reuse/overwrite] [reuse]: "
        );
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match input.trim().to_ascii_lowercase().as_str() {
            "" | "reuse" | "r" => return Ok(FeishuCredentialChoice::ReuseExisting),
            "overwrite" | "o" => return Ok(FeishuCredentialChoice::OverwriteWithLarkCli),
            _ => println!("Please enter `reuse` or `overwrite`."),
        }
    }
}

fn lark_cli_bin() -> String {
    std::env::var("REMI_LARK_CLI_BIN").unwrap_or_else(|_| "lark-cli".to_string())
}

fn lark_cli_installed(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

async fn fetch_auth_status(bin: &str) -> anyhow::Result<AuthStatusSummary> {
    let output = TokioCommand::new(bin)
        .args(["auth", "status"])
        .output()
        .await
        .with_context(|| format!("running `{bin} auth status`"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(AuthStatusSummary {
        success: output.status.success(),
        output: combined,
    })
}

async fn run_streaming_command(
    bin: &str,
    args: &[&str],
    url_hint: &str,
) -> anyhow::Result<CommandRunSummary> {
    run_streaming_command_with_stdin(bin, args, url_hint, None).await
}

async fn run_streaming_command_with_stdin(
    bin: &str,
    args: &[&str],
    url_hint: &str,
    stdin_input: Option<&str>,
) -> anyhow::Result<CommandRunSummary> {
    let mut child = TokioCommand::new(bin)
        .args(args)
        .stdin(if stdin_input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{bin} {}`", args.join(" ")))?;

    if let Some(input) = stdin_input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to capture stdin for `{bin}`"))?;
        stdin.write_all(input.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stdout from `{bin}`"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stderr from `{bin}`"))?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    tokio::spawn(read_stream_lines(stdout, tx.clone()));
    tokio::spawn(read_stream_lines(stderr, tx.clone()));
    drop(tx);

    let mut lines = Vec::new();
    let mut first_url = None;
    while let Some(line) = rx.recv().await {
        println!("{line}");
        if first_url.is_none() {
            if let Some(url) = extract_first_url(&line) {
                println!("{url_hint}");
                println!("{url}");
                first_url = Some(url);
            }
        }
        lines.push(line);
    }

    let status = child.wait().await?;
    Ok(CommandRunSummary {
        lines,
        first_url,
        success: status.success(),
    })
}

async fn read_stream_lines<R>(stream: R, tx: tokio::sync::mpsc::UnboundedSender<String>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let _ = tx.send(line);
    }
}

fn extract_first_url(line: &str) -> Option<String> {
    line.split_whitespace().find_map(normalize_possible_url)
}

fn normalize_possible_url(token: &str) -> Option<String> {
    let start = token.find("https://").or_else(|| token.find("http://"))?;
    let candidate = &token[start..];
    let trimmed = candidate.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';' | '.'
        )
    });
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn load_lark_cli_config_snapshot() -> anyhow::Result<Option<LarkCliConfigSnapshot>> {
    for path in lark_cli_config_candidates() {
        if !path.exists() {
            continue;
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading lark-cli config {}", path.display()))?;
        let json: serde_json::Value = serde_json::from_str(&raw)
            .with_context(|| format!("parsing lark-cli config {}", path.display()))?;
        let mut snapshot = extract_lark_cli_config_from_json(&json);
        snapshot.path = Some(path);
        return Ok(Some(snapshot));
    }
    Ok(None)
}

fn lark_cli_config_candidates() -> Vec<PathBuf> {
    if let Ok(path) = std::env::var("REMI_LARK_CONFIG_PATH") {
        return vec![PathBuf::from(path)];
    }

    let mut paths = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        paths.push(home.join(".lark-cli").join("config.json"));
        paths.push(home.join(".lark").join("config.json"));
        paths.push(home.join(".config").join("lark").join("config.json"));
        paths.push(
            home.join(".config")
                .join("larksuite-cli")
                .join("config.json"),
        );
        paths.push(
            home.join(".config")
                .join("configstore")
                .join("@larksuite")
                .join("cli.json"),
        );
    }
    paths
}

fn extract_lark_cli_config_from_json(json: &serde_json::Value) -> LarkCliConfigSnapshot {
    if let Some(apps) = json.get("apps").and_then(|value| value.as_array()) {
        for app in apps {
            let app_id = read_json_string(app, &["appId", "app_id", "cliAppId", "id"]);
            let app_secret =
                read_json_string(app, &["appSecret", "app_secret", "cliAppSecret", "secret"]);
            if app_id.is_some() || app_secret.is_some() {
                return LarkCliConfigSnapshot {
                    path: None,
                    app_id,
                    app_secret,
                };
            }
        }
    }

    LarkCliConfigSnapshot {
        path: None,
        app_id: read_json_string(json, &["appId", "app_id", "cliAppId", "id"]),
        app_secret: read_json_string(json, &["appSecret", "app_secret", "cliAppSecret", "secret"]),
    }
}

fn read_json_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(|v| v.as_str()) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

async fn run_setup_smoke(
    data_dir: &Path,
    config: &RuntimeConfig,
    api_key: &str,
) -> anyhow::Result<String> {
    unsafe {
        std::env::set_var("REMI_DATA_DIR", &config.data_dir);
        std::env::set_var("REMI_AGENT_ID", &config.root_agent_id);
        std::env::set_var("REMI_MODEL_PROFILE", &config.model_profile);
        std::env::set_var("REMI_SANDBOX_KIND", config.sandbox.kind.as_env_value());
        std::env::set_var(
            "REMI_SANDBOX_HOST_DIR",
            config.sandbox.host_dir_or_data_dir(&config.data_dir),
        );
        std::env::set_var("REMI_SANDBOX_CONTAINER_DIR", &config.sandbox.container_dir);
        std::env::set_var("REMI_SANDBOX_IMAGE", &config.sandbox.image);
        std::env::set_var(
            "REMI_SANDBOX_CONTAINER_NAME",
            &config.sandbox.container_name,
        );
        if !api_key.trim().is_empty() {
            std::env::set_var("OPENAI_API_KEY", api_key.trim());
        }
    }
    let bot = Rc::new(CatBotBuilder::from_env()?.build()?);
    let session_id = "setup-smoke";
    let mut output = String::new();
    let opts = StreamOptions::default();
    let mut stream = std::pin::pin!(bot.stream_with_options(
        session_id,
        Content::text(format!(
            "You are being verified for setup. Reply with one short sentence that says setup smoke passed for agent `{}`.",
            config.root_agent_id
        )),
        opts
    ));
    while let Some(event) = stream.next().await {
        match event {
            CatEvent::Text(delta) => output.push_str(&delta),
            CatEvent::Error(err) => anyhow::bail!(err.to_string()),
            CatEvent::Done => break,
            _ => {}
        }
    }
    if output.trim().is_empty() {
        anyhow::bail!("model returned an empty reply")
    }
    let _ = std::fs::remove_dir_all(data_dir.join("memory").join(session_id));
    Ok(output)
}

fn prompt_with_default(label: &str, default: &str) -> anyhow::Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value.to_string())
    }
}

fn choose_from_list(label: &str, options: &[String], default_id: &str) -> anyhow::Result<String> {
    println!("{label}:");
    for option in options {
        println!("  - {option}");
    }
    prompt_with_default("Enter id", default_id)
}

fn prompt_bool_with_default(label: &str, default: bool) -> anyhow::Result<bool> {
    let default_text = if default { "yes" } else { "no" };
    loop {
        let value = prompt_with_default(label, default_text)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" | "1" | "on" => return Ok(true),
            "n" | "no" | "false" | "0" | "off" => return Ok(false),
            _ => println!("Please enter yes or no."),
        }
    }
}

fn prompt_secret(current: Option<&str>) -> anyhow::Result<String> {
    let prompt = if current.is_some() {
        "OpenAI-compatible API key [leave blank to keep current]: "
    } else {
        "OpenAI-compatible API key: "
    };
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim().to_string();
    if value.is_empty() {
        Ok(current.unwrap_or_default().to_string())
    } else {
        Ok(value)
    }
}

async fn run_feishu(runtime: Rc<Runtime>, gateway: FeishuGateway) -> anyhow::Result<()> {
    info!("remi-cat single runtime starting Feishu gateway");
    let mut rx = match feishu_transport_from_env() {
        FeishuTransport::WebSocket => gateway.start().await?,
        FeishuTransport::EventHook => {
            gateway
                .start_event_hook(feishu_hook_config_from_env()?)
                .await?
        }
    };
    while let Some(event) = rx.recv().await {
        match event {
            FeishuEvent::MessageReceived(msg) => {
                let runtime = Rc::clone(&runtime);
                let gateway = gateway.clone();
                tokio::task::spawn_local(async move {
                    if let Err(err) = process_feishu_message(runtime, gateway, msg).await {
                        warn!("failed to process Feishu message: {err:#}");
                    }
                });
            }
            FeishuEvent::ReactionReceived(reaction) => {
                let text = format!("[user reacted with {}]", reaction.emoji_type);
                let msg = FeishuMessage {
                    message_id: reaction.message_id,
                    sender_user_id: reaction.sender_user_id,
                    chat_id: reaction.chat_id,
                    chat_type: "group".to_string(),
                    text,
                    images: Vec::new(),
                    files: Vec::new(),
                    documents: Vec::new(),
                    parent_id: None,
                    thread_id: reaction.thread_id,
                    at_bot: true,
                    mentions: Vec::new(),
                };
                let runtime = Rc::clone(&runtime);
                let gateway = gateway.clone();
                tokio::task::spawn_local(async move {
                    if let Err(err) = process_feishu_message(runtime, gateway, msg).await {
                        warn!("failed to process Feishu reaction: {err:#}");
                    }
                });
            }
            FeishuEvent::Unknown { event_type, .. } => {
                info!("ignored event type: {event_type}");
            }
            FeishuEvent::CardAction { .. } => {}
        }
    }
    Ok(())
}

fn im_mode_from_env() -> ImMode {
    match std::env::var("REMI_IM_MODE")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("disabled" | "off" | "none") => ImMode::Disabled,
        _ => ImMode::Feishu,
    }
}

fn feishu_transport_from_env() -> FeishuTransport {
    match std::env::var("REMI_FEISHU_TRANSPORT")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("event_hook" | "event-hook" | "hook" | "webhook") => FeishuTransport::EventHook,
        _ => FeishuTransport::WebSocket,
    }
}

fn feishu_hook_config_from_env() -> anyhow::Result<FeishuEventHookConfig> {
    let host = std::env::var("REMI_FEISHU_HOOK_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("REMI_FEISHU_HOOK_PORT")
        .unwrap_or_else(|_| "8788".to_string())
        .parse()
        .context("invalid REMI_FEISHU_HOOK_PORT")?;
    let path =
        std::env::var("REMI_FEISHU_HOOK_PATH").unwrap_or_else(|_| "/feishu/events".to_string());
    let verification_token = std::env::var("REMI_FEISHU_HOOK_VERIFICATION_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    Ok(FeishuEventHookConfig {
        addr: format!("{host}:{port}")
            .parse()
            .with_context(|| format!("invalid Feishu Event Hook address {host}:{port}"))?,
        path,
        verification_token,
    })
}

async fn run_cli(runtime: Rc<Runtime>, cli: CliConfig) -> anyhow::Result<()> {
    println!(
        "CLI IM ready. channel=`{}` user=`{}`. Type messages to chat, `quit` exits.",
        cli.channel_id, cli.user_id
    );
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    while let Some(line) = lines.next_line().await? {
        let text = line.trim().to_string();
        if text.is_empty() {
            continue;
        }
        if matches!(text.as_str(), "quit" | "exit") {
            break;
        }
        process_cli_message(Rc::clone(&runtime), &cli, text).await?;
    }
    Ok(())
}

async fn process_cli_message(
    runtime: Rc<Runtime>,
    cli: &CliConfig,
    text: String,
) -> anyhow::Result<()> {
    let msg = FeishuMessage {
        message_id: format!("cli-msg-{}", uuid::Uuid::new_v4()),
        sender_user_id: cli.user_id.clone(),
        chat_id: cli.channel_id.clone(),
        chat_type: "p2p".to_string(),
        text,
        images: Vec::new(),
        files: Vec::new(),
        documents: Vec::new(),
        parent_id: None,
        thread_id: None,
        at_bot: true,
        mentions: Vec::new(),
    };
    let reply =
        collect_bot_reply(runtime, CLI_CHANNEL, msg, Some(cli.username.clone()), None).await?;
    println!("{reply}");
    Ok(())
}

async fn process_prompt_message(
    runtime: Rc<Runtime>,
    cli: &CliConfig,
    text: String,
) -> anyhow::Result<()> {
    let session_id = runtime.sessions.lock().await.resolve_channel(
        "prompt",
        &cli.channel_id,
        &runtime.root_agent_id,
    )?;
    let (text, command_prefix, skill_injections) =
        match process_runtime_commands(&runtime, &session_id, text.trim()).await? {
            RuntimeCommandPipelineResult::Reply(reply) => {
                println!("{reply}");
                return Ok(());
            }
            RuntimeCommandPipelineResult::Continue {
                text,
                prefix,
                skill_injections,
            } => (text, prefix, skill_injections),
        };
    if !command_prefix.is_empty() {
        print!("{command_prefix}");
        io::stdout().flush()?;
    }
    let model_profile_id = runtime
        .sessions
        .lock()
        .await
        .metadata_string(&session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
    let opts = StreamOptions {
        model_profile_id,
        skill_injections,
        ..StreamOptions::default()
    };
    let mut stream =
        std::pin::pin!(runtime
            .bot
            .stream_with_options(&session_id, Content::text(text), opts));
    let mut output = String::new();
    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else { break };
                match event {
                    CatEvent::Text(delta) => {
                        print!("{delta}");
                        io::stdout().flush()?;
                        output.push_str(&delta);
                    }
                    CatEvent::Error(err) => anyhow::bail!(err.to_string()),
                    CatEvent::Done => break,
                    _ => {}
                }
            }
            _ = &mut timeout => {
                anyhow::bail!("prompt timed out");
            }
        }
    }
    if !output.ends_with('\n') {
        println!();
    }
    Ok(())
}

async fn process_feishu_message(
    runtime: Rc<Runtime>,
    gateway: FeishuGateway,
    msg: FeishuMessage,
) -> anyhow::Result<()> {
    let channel_id = feishu_session_channel_id(&msg);
    let session_exists = runtime
        .sessions
        .lock()
        .await
        .channel_session_id(FEISHU_CHANNEL, &channel_id)
        .is_some();
    if should_ignore_unaddressed_topic_start(&msg, session_exists) {
        info!(
            chat_id = %msg.chat_id,
            thread_id = msg.thread_id.as_deref().unwrap_or(""),
            message_id = %msg.message_id,
            "ignored topic message because the topic session has not been started by an @mention"
        );
        return Ok(());
    }

    let sender_uuid = runtime
        .user_store
        .resolve_or_create(FEISHU_CHANNEL, &msg.sender_user_id);
    let sender_username = ensure_im_username(
        &runtime.user_store,
        &gateway,
        &sender_uuid,
        &msg.sender_user_id,
    )
    .await;
    let reaction_id = gateway.add_reaction(&msg.message_id, "THINKING").await.ok();
    let mut replies = FeishuReplyStream::new(gateway.clone(), msg.message_id.clone());
    collect_bot_reply(
        runtime,
        FEISHU_CHANNEL,
        msg.clone(),
        sender_username,
        Some(&mut replies),
    )
    .await?;
    replies.finish().await;
    if let Some(reaction_id) = reaction_id {
        gateway
            .delete_reaction(&msg.message_id, &reaction_id)
            .await
            .ok();
    }
    Ok(())
}

async fn collect_bot_reply(
    runtime: Rc<Runtime>,
    platform: &str,
    mut msg: FeishuMessage,
    sender_username: Option<String>,
    mut replies: Option<&mut FeishuReplyStream>,
) -> anyhow::Result<String> {
    let channel_id = feishu_session_channel_id(&msg);
    let session_id = runtime.sessions.lock().await.resolve_channel(
        platform,
        &channel_id,
        &runtime.root_agent_id,
    )?;
    if platform == FEISHU_CHANNEL && is_fork_command(msg.text.trim()) {
        let reply = handle_feishu_fork_command(&runtime, &session_id, &msg).await?;
        append_reply_chunk(
            &mut String::new(),
            &mut replies,
            FeishuReplyKind::Text,
            &reply,
        )
        .await;
        return Ok(reply);
    }
    let (command_prefix, skill_injections) =
        match process_runtime_commands(&runtime, &session_id, msg.text.trim()).await? {
            RuntimeCommandPipelineResult::Reply(reply) => {
                append_reply_chunk(
                    &mut String::new(),
                    &mut replies,
                    FeishuReplyKind::Text,
                    &reply,
                )
                .await;
                return Ok(reply);
            }
            RuntimeCommandPipelineResult::Continue {
                text,
                prefix,
                skill_injections: injections,
            } => {
                msg.text = text;
                (prefix, injections)
            }
        };

    let im_attachments = msg
        .files
        .iter()
        .map(|f| ImAttachment {
            key: encode_agent_file_key(&msg.message_id, &f.file_key),
            name: f.file_name.clone(),
            mime_type: f.mime_type.clone(),
            size_bytes: f.size_bytes,
            file_type: f.file_type.clone(),
        })
        .collect();
    let im_documents = msg
        .documents
        .iter()
        .map(|d| ImDocument {
            url: d.url.clone(),
            title: d.title.clone(),
            doc_type: d.doc_type.clone(),
            token: d.token.clone(),
        })
        .collect();
    let model_profile_id = runtime
        .sessions
        .lock()
        .await
        .metadata_string(&session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
    let opts = StreamOptions {
        model_profile_id,
        skill_injections,
        sender_user_id: Some(msg.sender_user_id.clone()),
        sender_username,
        message_id: Some(msg.message_id.clone()),
        chat_type: Some(msg.chat_type.clone()),
        platform: Some(platform.to_string()),
        todo_create_via_sdk: true,
        trigger_tools_enabled: true,
        im_attachments,
        im_documents,
        ..StreamOptions::default()
    };
    let content = build_message_content(
        &msg.text,
        &[],
        !msg.images.is_empty(),
        msg.files.len(),
        msg.documents.len(),
    );
    let mut output = command_prefix;
    let mut streaming_tool_names = HashMap::<String, String>::new();
    let debug_enabled = runtime
        .sessions
        .lock()
        .await
        .metadata_bool(&session_id, SESSION_DEBUG_METADATA_KEY);
    if !output.is_empty() {
        if let Some(replies) = replies.as_deref_mut() {
            replies.push(FeishuReplyKind::Text, &output).await;
        }
    }
    let mut supervisor_execution_started = false;
    let mut stream = std::pin::pin!(runtime.bot.stream_with_options(&session_id, content, opts));
    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else { break };
                match event {
                    CatEvent::Text(delta) => {
                        supervisor_execution_started = false;
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Text, &delta).await;
                    }
                    CatEvent::Thinking(content) => {
                        let chunk = format!("\n\n**Thinking**\n{}\n", fenced_block("text", &content));
                        supervisor_execution_started = false;
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Thinking, &chunk).await;
                    }
                    CatEvent::ToolCallStart { id, name } => {
                        streaming_tool_names.insert(id.clone(), name.clone());
                        let pretty = bot_core::PrettyToolCall::started(
                            &id,
                            &name,
                            &serde_json::Value::Object(serde_json::Map::new()),
                        );
                        let line = format_feishu_tool_line(&pretty);
                        supervisor_execution_started = false;
                        update_tool_reply(&mut output, &mut replies, &id, &line, false).await;
                    }
                    CatEvent::ToolCallArgumentsDelta { .. } => {}
                    CatEvent::ToolCall { id, name, args } => {
                        streaming_tool_names.remove(&id);
                        let pretty = bot_core::PrettyToolCall::started(&id, &name, &args);
                        let line = format_feishu_tool_line(&pretty);
                        supervisor_execution_started = false;
                        update_tool_reply(&mut output, &mut replies, &id, &line, false).await;
                    }
                    CatEvent::ToolCallResult {
                        id,
                        name,
                        args,
                        result,
                        success,
                        elapsed_ms,
                    } => {
                        streaming_tool_names.remove(&id);
                        let pretty = bot_core::PrettyToolCall::completed(
                            &id, &name, &args, &result, success, elapsed_ms,
                        );
                        let line = format_feishu_tool_line(&pretty);
                        supervisor_execution_started = false;
                        update_tool_reply(&mut output, &mut replies, &id, &line, true).await;
                    }
                    CatEvent::SubSession(event) => {
                        record_sub_session_event(&runtime, &session_id, platform, &msg, &event).await;
                        let chunk = format!(
                            "\n\n**Sub-session** `{}` / `{}`\n",
                            event.agent_name, event.sub_thread_id.0
                        );
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::SubSession, &chunk).await;
                    }
                    CatEvent::SupervisorProgress(progress) => {
                        let reply_kind = supervisor_reply_kind(&progress);
                        let mut chunk = String::new();
                        if !supervisor_execution_started {
                            chunk.push_str("\n\n---\n\n**Supervisor execution**\n");
                            supervisor_execution_started = true;
                        }
                        chunk.push_str(&format_supervisor_progress(&progress));
                        append_reply_chunk(&mut output, &mut replies, reply_kind, &chunk).await;
                    }
                    CatEvent::Supervisor(report) => {
                        let context = runtime.bot.workflow_status(&session_id).await.map(|instance| instance.context).unwrap_or(serde_json::Value::Null);
                        let chunk = bot_core::supervisor_workflow::format_prefix(&report, &context);
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Supervisor, &chunk).await;
                        supervisor_execution_started = false;
                    }
                    CatEvent::ContextCompaction(event) => {
                        let line = format_context_compaction_line(&event);
                        let done = !matches!(event.status, bot_core::ContextCompactionStatus::Started);
                        update_context_compaction_reply(&mut output, &mut replies, &event.id, &line, done).await;
                        supervisor_execution_started = false;
                    }
                    CatEvent::Stats {
                        prompt_tokens,
                        completion_tokens,
                        max_prompt_tokens,
                        elapsed_ms,
                    } => {
                        if !debug_enabled {
                            continue;
                        }
                        let model_profile_id = runtime
                            .sessions
                            .lock()
                            .await
                            .metadata_string(&session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
                        let context_tokens = runtime
                            .bot
                            .model_context_tokens_for(model_profile_id.as_deref());
                        let context_percent = if context_tokens == 0 {
                            0.0
                        } else {
                            max_prompt_tokens as f64 / context_tokens as f64 * 100.0
                        };
                        let chunk = format!(
                            "\n\n---\n**调试信息**\n\n**Stats** `tokens: {prompt_tokens}->{completion_tokens}` `context: {max_prompt_tokens}/{context_tokens} ({context_percent:.1}%)` `elapsed: {elapsed_ms}ms`"
                        );
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Stats, &chunk).await;
                    }
                    CatEvent::Error(err) => {
                        let chunk = format!(
                            "\n\n---\n**调试信息**\n\n**Error**\n{}",
                            fenced_block("text", &err.to_string())
                        );
                        append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Error, &chunk).await;
                        break;
                    }
                    CatEvent::Done => break,
                    _ => {}
                }
            }
            _ = &mut timeout => {
                let chunk = "\n\n---\n**调试信息**\n\n**Timeout** reply timed out";
                append_reply_chunk(&mut output, &mut replies, FeishuReplyKind::Error, chunk).await;
                break;
            }
        }
    }
    if output.trim().is_empty() {
        append_reply_chunk(
            &mut output,
            &mut replies,
            FeishuReplyKind::Text,
            "（无响应）",
        )
        .await;
    }
    Ok(output)
}

pub(crate) enum RuntimeCommandResult {
    Reply(String),
    Continue {
        text: String,
        prefix: String,
        skill_injections: Vec<SkillDocument>,
    },
}

pub(crate) enum RuntimeCommandPipelineResult {
    Reply(String),
    Continue {
        text: String,
        prefix: String,
        skill_injections: Vec<SkillDocument>,
    },
}

pub(crate) async fn process_runtime_commands(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<RuntimeCommandPipelineResult> {
    let mut text = command.trim().to_string();
    let mut prefix = String::new();
    let mut skill_injections = Vec::new();
    for _ in 0..MAX_COMMAND_PREPROCESS_DEPTH {
        match handle_runtime_command(runtime, session_id, text.trim()).await? {
            Some(RuntimeCommandResult::Reply(reply)) => {
                prefix.push_str(&reply);
                return Ok(RuntimeCommandPipelineResult::Reply(prefix));
            }
            Some(RuntimeCommandResult::Continue {
                text: next_text,
                prefix: next_prefix,
                skill_injections: mut next_skill_injections,
            }) => {
                prefix.push_str(&next_prefix);
                skill_injections.append(&mut next_skill_injections);
                text = next_text;
                if text.trim().is_empty() {
                    return Ok(RuntimeCommandPipelineResult::Reply(prefix));
                }
            }
            None => {
                return Ok(RuntimeCommandPipelineResult::Continue {
                    text,
                    prefix,
                    skill_injections,
                });
            }
        }
    }
    Ok(RuntimeCommandPipelineResult::Reply(
        "command preprocessing exceeded the maximum nesting depth".to_string(),
    ))
}

fn is_fork_command(command: &str) -> bool {
    command == "/fork" || command.starts_with("/fork ")
}

async fn handle_feishu_fork_command(
    runtime: &Runtime,
    source_session_id: &str,
    msg: &FeishuMessage,
) -> anyhow::Result<String> {
    if runtime.bot.is_thread_running(source_session_id).await {
        return Ok("当前 session 正在运行，结束或取消后再 fork。".to_string());
    }
    let title = msg
        .text
        .trim()
        .strip_prefix("/fork")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let temporary_channel_id = format!("fork:{}", uuid::Uuid::new_v4());
    let fork = runtime
        .sessions
        .lock()
        .await
        .fork_session(
            source_session_id,
            FEISHU_CHANNEL,
            &temporary_channel_id,
            title,
        )?
        .ok_or_else(|| anyhow::anyhow!("source session `{source_session_id}` not found"))?;
    if let Err(error) = runtime
        .bot
        .fork_thread_data(source_session_id, &fork.id, Some(&msg.sender_user_id))
        .await
    {
        let _ = runtime.sessions.lock().await.delete(&fork.id);
        return Err(anyhow::Error::from(error));
    }

    let binding = runtime
        .im_bridge
        .sub_session_binding_upsert(SubSessionBindingUpsertRequest {
            parent_session_id: source_session_id.to_string(),
            sub_session_id: fork.id.clone(),
            kind: "fork".to_string(),
            target: "session".to_string(),
            title: fork.title.clone(),
            platform: FEISHU_CHANNEL.to_string(),
            parent_channel_id: msg.chat_id.clone(),
            parent_thread_id: msg.thread_id.clone(),
            actor_user_id: Some(msg.sender_user_id.clone()),
        })
        .await;

    match binding {
        Ok(Some(binding)) => {
            runtime.sessions.lock().await.set_channel_binding(
                &fork.id,
                &binding.platform,
                &binding.channel_id,
            )?;
            Ok(format!(
                "已 fork 当前 session。\n\n新 session: `{}`\n标题: {}\n已创建新的飞书子会话入口。",
                fork.id,
                fork.title.as_deref().unwrap_or("新对话")
            ))
        }
        Ok(None) => Ok(format!(
            "已 fork 当前 session。\n\n新 session: `{}`\n标题: {}\n未创建飞书子会话入口，可通过 session id 访问。",
            fork.id,
            fork.title.as_deref().unwrap_or("新对话")
        )),
        Err(error) => Ok(format!(
            "已 fork 当前 session，但创建飞书子会话入口失败。\n\n新 session: `{}`\n标题: {}\n错误: {error:#}",
            fork.id,
            fork.title.as_deref().unwrap_or("新对话")
        )),
    }
}

pub(crate) async fn handle_runtime_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<Option<RuntimeCommandResult>> {
    if command == "/tools" {
        let reply = runtime
            .bot
            .tool_list()
            .into_iter()
            .map(|(name, desc)| format!("- `{name}`: {desc}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/skill" || command.starts_with("/skill ") || command.starts_with("/skill:") {
        let result = handle_skill_command(runtime, session_id, command).await?;
        return Ok(Some(result));
    }
    if command.starts_with("/debug") {
        let reply = handle_debug_command(runtime, session_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command.starts_with("/secrets") || command.starts_with("/secret") {
        let args = command
            .trim_start_matches('/')
            .split_whitespace()
            .skip(1)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let command = parse_secret_command(&args)?;
        let reply = handle_runtime_secret_command(runtime, &command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command.starts_with("/goal") {
        let reply = handle_goal_command(&runtime.bot, session_id, command).await?;
        if is_goal_set_command(command) && reply.starts_with("已设置 goal。") {
            let goal = runtime
                .bot
                .goal_status(session_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("goal was not persisted after set"))?;
            return Ok(Some(RuntimeCommandResult::Continue {
                text: goal.goal,
                prefix: format!("{reply}\n\n---\n\n"),
                skill_injections: Vec::new(),
            }));
        }
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command.starts_with("/workflow") {
        let reply = handle_workflow_command(&runtime.bot, session_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/model" || command.starts_with("/model ") {
        let reply = handle_model_command(runtime, session_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/permissions"
        || command.starts_with("/permissions ")
        || command == "/permission"
        || command.starts_with("/permission ")
    {
        let reply = handle_permissions_command(runtime, session_id, command).await?;
        return Ok(Some(RuntimeCommandResult::Reply(reply)));
    }
    if command == "/compact" {
        let n = runtime.bot.compact_memory(session_id).await?;
        return Ok(Some(RuntimeCommandResult::Reply(format!(
            "compacted {n} short-term message(s)."
        ))));
    }
    if command == "/clear" {
        runtime.bot.clear_memory(session_id).await?;
        return Ok(Some(RuntimeCommandResult::Reply(
            "已清空当前 session 的历史会话。Todo/trigger 等工具状态已保留。".to_string(),
        )));
    }
    if command == "/doctor" {
        return Ok(Some(RuntimeCommandResult::Reply(command_doctor_report(
            runtime,
        ))));
    }
    if command == "/usage" {
        let model_profile_id = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        let usage = runtime
            .bot
            .account_usage_for(model_profile_id.as_deref())
            .await?;
        return Ok(Some(RuntimeCommandResult::Reply(format_account_usage(
            &usage,
        ))));
    }
    Ok(None)
}

async fn handle_permissions_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    use bot_core::approval::ApprovalSessionPolicy;

    let rest = command
        .trim()
        .trim_start_matches('/')
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");
    let manager = runtime.bot.approval_manager();

    let policy = match rest {
        "" | "status" => None,
        "ask" | "default" | "prompt" | "reset" => Some(ApprovalSessionPolicy::Ask),
        "auto" | "medium" | "model-auto" | "model_auto" => Some(ApprovalSessionPolicy::ModelAuto),
        "allow" | "allow-session" | "allow_session" | "bypass" | "trusted" => {
            Some(ApprovalSessionPolicy::AllowAll)
        }
        _ => {
            return Ok(format!(
                "用法：/permissions status | ask | auto | allow\n\n{}",
                format_permissions_status(session_id, manager.session_policy(session_id).await)
            ));
        }
    };

    if let Some(policy) = policy {
        manager.set_session_policy(session_id, policy).await;
    }

    Ok(format_permissions_status(
        session_id,
        manager.session_policy(session_id).await,
    ))
}

fn format_permissions_status(
    session_id: &str,
    policy: bot_core::approval::ApprovalSessionPolicy,
) -> String {
    format!(
        "**permissions**\n\nsession_id: `{session_id}`\nmode: `{}`\n{}\n\nmodes:\n- `ask`: low 自动通过；medium/high 请求审批\n- `auto` / `medium`: low/medium 自动通过；high 请求审批\n- `allow`: 本 session 所有 tool request 自动通过",
        policy.label(),
        policy.description()
    )
}

async fn handle_model_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    let rest = command.trim().strip_prefix("/model").unwrap_or("").trim();
    if rest.is_empty() || rest == "status" {
        let stored = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        return Ok(format_model_status(
            &runtime.bot,
            stored.as_deref(),
            session_id,
        ));
    }
    if rest == "list" {
        let stored = runtime
            .sessions
            .lock()
            .await
            .metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY);
        return Ok(format_model_list(&runtime.bot, stored.as_deref()));
    }
    if rest == "reset" {
        runtime
            .sessions
            .lock()
            .await
            .remove_metadata(session_id, SESSION_MODEL_PROFILE_METADATA_KEY)?;
        return Ok(format!(
            "已清除当前 session 的模型 override。\n\n{}",
            format_model_status(&runtime.bot, None, session_id)
        ));
    }
    if let Some(id) = rest.strip_prefix("use").map(str::trim) {
        if id.is_empty() {
            anyhow::bail!("用法：/model use <profile_id>");
        }
        let Some(profile) = runtime.bot.get_model_profile(id) else {
            anyhow::bail!(
                "unknown model profile `{id}`. Use `/model list` to see available profiles."
            );
        };
        let profile_id = profile.id.clone();
        runtime.sessions.lock().await.set_metadata_string(
            session_id,
            SESSION_MODEL_PROFILE_METADATA_KEY,
            &profile_id,
        )?;
        return Ok(format!(
            "已切换当前 session 模型为 `{}`。\n\n{}",
            profile_id,
            format_model_status(&runtime.bot, Some(&profile_id), session_id)
        ));
    }
    Ok("用法：/model status，/model list，/model use <profile_id>，/model reset".to_string())
}

fn format_model_status(
    bot: &CatBot,
    stored_model_profile_id: Option<&str>,
    session_id: &str,
) -> String {
    let effective = bot.effective_model_profile(stored_model_profile_id);
    let source = if effective.invalid_session_model.is_none()
        && stored_model_profile_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some()
    {
        "session"
    } else {
        "default"
    };
    let mut lines = vec![
        "**model status**".to_string(),
        String::new(),
        format!("session_id: `{session_id}`"),
        format!("effective_model_profile: `{}`", effective.profile.id),
        format!("source: `{source}`"),
        format!("name: {}", effective.profile.name),
        format!(
            "provider: `{}`",
            effective.profile.provider.as_deref().unwrap_or("unknown")
        ),
        format!("model: `{}`", effective.profile.model),
        format!(
            "base_url: `{}`",
            effective.profile.base_url.as_deref().unwrap_or("-")
        ),
        format!("context_tokens: {}", effective.profile.context_tokens),
        format!("max_output_tokens: {}", effective.profile.max_output_tokens),
        format!("supports_images: {}", effective.profile.supports_images),
    ];
    if let Some(stored) = stored_model_profile_id {
        lines.push(format!("session_override: `{stored}`"));
    } else {
        lines.push("session_override: none".to_string());
    }
    if let Some(invalid) = effective.invalid_session_model {
        lines.push(format!(
            "warning: stored session override `{invalid}` is invalid; using fallback `{}`",
            effective.profile.id
        ));
    }
    lines.join("\n")
}

fn format_model_list(bot: &CatBot, stored_model_profile_id: Option<&str>) -> String {
    let effective = bot.effective_model_profile(stored_model_profile_id);
    let default_id = &bot.default_model_profile().id;
    let mut profiles = bot.model_profiles();
    profiles.sort_by(|a, b| a.id.cmp(&b.id));
    let mut lines = vec!["**model profiles**".to_string(), String::new()];
    for profile in profiles {
        let mut markers = Vec::new();
        if profile.id == effective.profile.id {
            markers.push("current");
        }
        if &profile.id == default_id {
            markers.push("default");
        }
        let marker = if markers.is_empty() {
            String::new()
        } else {
            format!(" [{}]", markers.join(", "))
        };
        lines.push(format!(
            "- `{}`{}: {} / `{}` / context={} / images={}",
            profile.id,
            marker,
            profile.provider.as_deref().unwrap_or("unknown"),
            profile.model,
            profile.context_tokens,
            profile.supports_images
        ));
    }
    if let Some(invalid) = effective.invalid_session_model {
        lines.push(format!(
            "\nwarning: stored session override `{invalid}` is invalid; using fallback `{}`",
            effective.profile.id
        ));
    }
    lines.join("\n")
}

async fn handle_skill_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<RuntimeCommandResult> {
    let trimmed = command.trim();
    if trimmed == "/skill" || trimmed == "/skill list" {
        return Ok(RuntimeCommandResult::Reply(format_skill_list(&runtime.bot)));
    }
    if trimmed == "/skill status" {
        let names = runtime.bot.read_skill_names(session_id).await;
        return Ok(RuntimeCommandResult::Reply(format_skill_status(&names)));
    }
    if trimmed.starts_with("/skill:") {
        let (skills, rest) = parse_skill_prefixes(&runtime.bot, trimmed).await?;
        if rest.trim().is_empty() {
            let names = skills
                .iter()
                .map(|skill| format!("`{}`", skill.name))
                .collect::<Vec<_>>()
                .join(", ");
            return Ok(RuntimeCommandResult::Reply(format!(
                "已加载 skill {names}。请在同一条消息中提供要执行的任务。"
            )));
        }
        let prefix = skills
            .iter()
            .map(|skill| format!("已加载 skill `{}`。\n", skill.name))
            .collect::<String>();
        return Ok(RuntimeCommandResult::Continue {
            text: rest.trim().to_string(),
            prefix,
            skill_injections: skills,
        });
    }
    Ok(RuntimeCommandResult::Reply(
        "用法：/skill list，/skill status，/skill:<name> <任务>".to_string(),
    ))
}

async fn parse_skill_prefixes(
    bot: &CatBot,
    mut text: &str,
) -> anyhow::Result<(Vec<SkillDocument>, String)> {
    let mut skills = Vec::new();
    loop {
        let Some(rest) = text.trim_start().strip_prefix("/skill:") else {
            break;
        };
        let rest = rest.trim_start();
        let split_at = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let name = rest[..split_at].trim();
        if name.is_empty() {
            anyhow::bail!("用法：/skill:<name> <任务>");
        }
        let Some(doc) = bot.get_skill(name).await? else {
            anyhow::bail!("Skill `{name}` not found. Use `/skill list` to see available skills.");
        };
        skills.push(doc);
        text = &rest[split_at..];
    }
    Ok((skills, text.to_string()))
}

fn format_skill_list(bot: &CatBot) -> String {
    let mut skills = bot.skill_summaries();
    skills.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.source.cmp(&b.source)));
    if skills.is_empty() {
        return "No skills are available.".to_string();
    }
    let mut lines = vec!["**skills**".to_string(), String::new()];
    for skill in skills {
        lines.push(format!(
            "- `/skill:{}` - {} ({})",
            skill.name, skill.description, skill.source
        ));
    }
    lines.join("\n")
}

fn format_skill_status(names: &[String]) -> String {
    if names.is_empty() {
        return "当前 session 尚未读取 skill。".to_string();
    }
    format!(
        "**read skills**\n\n{}",
        names
            .iter()
            .map(|name| format!("- `{name}`"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn format_account_usage(usage: &AccountUsage) -> String {
    let mut lines = vec![
        "**model usage**".to_string(),
        String::new(),
        format!("provider: `{}`", usage.provider),
        format!("model_profile: `{}`", usage.model_profile_id),
        format!("model: `{}`", usage.model),
    ];

    match &usage.status {
        AccountUsageStatus::Unknown { reason } => {
            lines.push("status: `unknown`".to_string());
            lines.push(format!("reason: {reason}"));
        }
        AccountUsageStatus::Available { balances } => {
            lines.push("status: `available`".to_string());
            if balances.is_empty() {
                lines.push("balance: none returned".to_string());
            } else {
                for balance in balances {
                    lines.push(format_balance_line(balance));
                }
            }
        }
    }

    lines.join("\n")
}

fn format_balance_line(balance: &AccountBalance) -> String {
    let mut parts = Vec::new();
    if let Some(currency) = &balance.currency {
        parts.push(format!("currency={currency}"));
    }
    if let Some(total) = &balance.total {
        parts.push(format!("total={total}"));
    }
    if let Some(granted) = &balance.granted {
        parts.push(format!("granted={granted}"));
    }
    if let Some(topped_up) = &balance.topped_up {
        parts.push(format!("topped_up={topped_up}"));
    }
    if let Some(voucher) = &balance.voucher {
        parts.push(format!("voucher={voucher}"));
    }
    if let Some(cash) = &balance.cash {
        parts.push(format!("cash={cash}"));
    }
    if let Some(available) = balance.available {
        parts.push(format!("available={available}"));
    }

    if parts.is_empty() {
        "- balance: unknown".to_string()
    } else {
        format!("- balance: {}", parts.join(", "))
    }
}

async fn handle_debug_command(
    runtime: &Runtime,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    let rest = command.trim().strip_prefix("/debug").unwrap_or("").trim();
    match rest {
        "" | "status" => {
            let enabled = runtime
                .sessions
                .lock()
                .await
                .metadata_bool(session_id, SESSION_DEBUG_METADATA_KEY);
            Ok(format!(
                "debug 信息当前为：{}",
                if enabled { "开启" } else { "关闭" }
            ))
        }
        "on" | "enable" | "enabled" | "true" | "1" => {
            runtime.sessions.lock().await.set_metadata_bool(
                session_id,
                SESSION_DEBUG_METADATA_KEY,
                true,
            )?;
            Ok("已开启本 session 的 debug 信息。之后每轮会返回最后的 Stats 调试块。".to_string())
        }
        "off" | "disable" | "disabled" | "false" | "0" => {
            runtime.sessions.lock().await.set_metadata_bool(
                session_id,
                SESSION_DEBUG_METADATA_KEY,
                false,
            )?;
            Ok("已关闭本 session 的 debug 信息。之后不再返回最后的 Stats 调试块。".to_string())
        }
        _ => Ok("用法：/debug on，/debug off，/debug status".to_string()),
    }
}

async fn handle_workflow_command(
    bot: &CatBot,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    let rest = command
        .trim()
        .strip_prefix("/workflow")
        .unwrap_or("")
        .trim();
    if rest.is_empty() || rest == "status" {
        return Ok(match bot.workflow_status(session_id).await {
            Some(instance) => format_workflow_status(&instance),
            None => "当前 session 没有 supervisor workflow。".to_string(),
        });
    }
    if rest == "pause" || rest == "stop" {
        bot.pause_workflow(session_id).await?;
        return Ok("已暂停当前 session 的 supervisor workflow。".to_string());
    }
    if rest == "clear" {
        bot.clear_workflow(session_id).await?;
        return Ok("已清除当前 session 的 supervisor workflow。".to_string());
    }
    let set_args = rest
        .strip_prefix("set")
        .or_else(|| rest.strip_prefix("start"))
        .map(str::trim);
    let Some(set_args) = set_args else {
        return Ok("用法：/workflow set <id> [--max-rounds N|unlimited] [--context {JSON}]，/workflow pause，/workflow clear，/workflow status".to_string());
    };
    let mut split = set_args.splitn(2, char::is_whitespace);
    let workflow_id = split.next().unwrap_or("").trim();
    let mut options = split.next().unwrap_or("").trim();
    if workflow_id.is_empty() {
        return Ok("用法：/workflow set <id> ...".to_string());
    }
    let mut max_rounds = GoalMaxRounds::default();
    let mut context = serde_json::json!({});
    while !options.is_empty() {
        if let Some(rest) = options.strip_prefix("--max-rounds") {
            let rest = rest.trim_start();
            let mut parts = rest.splitn(2, char::is_whitespace);
            max_rounds = parse_goal_max_rounds(parts.next().unwrap_or(""))?;
            options = parts.next().unwrap_or("").trim();
            continue;
        }
        if let Some(rest) = options.strip_prefix("--context") {
            let raw = rest.trim_start();
            context = serde_json::from_str(raw).context("invalid workflow context JSON")?;
            options = "";
            continue;
        }
        anyhow::bail!("unknown workflow option: {options}");
    }
    let instance = bot
        .start_workflow_by_id(session_id, workflow_id, context, max_rounds)
        .await?;
    Ok(format!(
        "已设置 supervisor workflow。\n\n{}",
        format_workflow_status(&instance)
    ))
}

fn format_workflow_status(instance: &bot_core::WorkflowInstance) -> String {
    let max_rounds = match &instance.max_rounds {
        bot_core::WorkflowMaxRounds::Limited(value) => value.to_string(),
        bot_core::WorkflowMaxRounds::Unlimited => "unlimited".to_string(),
    };
    let mut text = format!(
        "workflow: {}\nstatus: {:?}\nnode: {}\nmax_rounds: {}\ncontext: {}",
        instance.definition.id,
        instance.status,
        instance.current_node,
        max_rounds,
        serde_json::to_string(&instance.context).unwrap_or_else(|_| "{}".into())
    );
    if let Some(report) = &instance.last_report {
        if let Some(edge) = &report.edge {
            text.push_str(&format!(
                "\nlast_transition: {} -> {} via {}\nlast_reason: {}",
                report.from_node, report.to_node, edge, report.reason
            ));
        } else {
            text.push_str(&format!(
                "\nlast_state: {:?} at {}\nlast_reason: {}",
                report.status, report.to_node, report.reason
            ));
        }
        if let Some(message) = &report.agent_message {
            text.push_str(&format!("\nlast_agent_message: {message}"));
        }
        if let Some(message) = &report.next_node_message {
            text.push_str(&format!("\nnext_node_message: {message}"));
        }
    }
    text
}

async fn handle_goal_command(
    bot: &CatBot,
    session_id: &str,
    command: &str,
) -> anyhow::Result<String> {
    let rest = command.trim().strip_prefix("/goal").unwrap_or("").trim();
    if rest.is_empty() || rest == "status" {
        return Ok(match bot.goal_status(session_id).await {
            Some(goal) => format_goal_status(&goal),
            None => "当前 session 没有设置 goal。".to_string(),
        });
    }
    if rest == "clear" {
        bot.clear_goal(session_id).await?;
        return Ok("已清除当前 session 的 goal。".to_string());
    }
    if rest == "pause" {
        bot.pause_workflow(session_id).await?;
        return Ok("已暂停当前 session 的 goal。".to_string());
    }
    let Some(mut goal_text) = rest.strip_prefix("set").map(str::trim) else {
        return Ok(
            "用法：/goal set [--max-rounds N|unlimited] <目标>，/goal pause，/goal clear，/goal status".to_string(),
        );
    };
    let mut max_rounds = GoalMaxRounds::default();
    if let Some(after_flag) = goal_text.strip_prefix("--max-rounds") {
        let after_flag = after_flag.trim_start();
        let mut parts = after_flag.splitn(2, char::is_whitespace);
        let raw_limit = parts.next().unwrap_or("").trim();
        let remaining = parts.next().unwrap_or("").trim();
        if raw_limit.is_empty() || remaining.is_empty() {
            return Ok("用法：/goal set --max-rounds <N|unlimited> <目标>".to_string());
        }
        max_rounds = parse_goal_max_rounds(raw_limit)?;
        goal_text = remaining;
    }
    if goal_text.trim().is_empty() {
        return Ok("用法：/goal set [--max-rounds N|unlimited] <目标>".to_string());
    }
    let goal = bot.set_goal(session_id, goal_text, max_rounds).await?;
    Ok(format!("已设置 goal。\n\n{}", format_goal_status(&goal)))
}

fn is_goal_set_command(command: &str) -> bool {
    command
        .trim()
        .strip_prefix("/goal")
        .map(str::trim)
        .and_then(|rest| rest.strip_prefix("set"))
        .is_some_and(|rest| rest.is_empty() || rest.chars().next().is_some_and(char::is_whitespace))
}

fn parse_goal_max_rounds(raw: &str) -> anyhow::Result<GoalMaxRounds> {
    if raw.eq_ignore_ascii_case("unlimited") || raw == "无限" {
        return Ok(GoalMaxRounds::Unlimited);
    }
    let value: u32 = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("--max-rounds must be a positive integer or unlimited"))?;
    if value == 0 {
        anyhow::bail!("--max-rounds must be greater than 0");
    }
    Ok(GoalMaxRounds::Limited(value))
}

fn format_goal_status(goal: &bot_core::GoalState) -> String {
    let status = match &goal.status {
        bot_core::GoalStatus::Active => "active",
        bot_core::GoalStatus::Paused => "paused",
        bot_core::GoalStatus::Completed => "completed",
    };
    let max_rounds = match &goal.max_rounds {
        GoalMaxRounds::Limited(value) => value.to_string(),
        GoalMaxRounds::Unlimited => "unlimited".to_string(),
    };
    let last = goal
        .last_evaluation
        .as_ref()
        .map(|decision| {
            let status = match &decision.status {
                bot_core::goal::SupervisorDecisionStatus::Completed => "completed",
                bot_core::goal::SupervisorDecisionStatus::Continue => "continue",
            };
            format!("\nlast_supervisor: {status} - {}", decision.reason)
        })
        .unwrap_or_default();
    format!(
        "goal: {}\nstatus: {status}\nmax_rounds: {max_rounds}{last}",
        goal.goal
    )
}

fn feishu_session_channel_id(msg: &FeishuMessage) -> String {
    let thread_id = msg
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if msg.chat_type == "group" {
        if let Some(thread_id) = thread_id {
            return feishu_topic_channel_id(&msg.chat_id, thread_id);
        }
    }
    msg.chat_id.clone()
}

fn feishu_topic_channel_id(chat_id: &str, thread_id: &str) -> String {
    format!("{}:thread:{}", chat_id.trim(), thread_id.trim())
}

fn should_ignore_unaddressed_topic_start(msg: &FeishuMessage, session_exists: bool) -> bool {
    msg.chat_type == "group"
        && msg
            .thread_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        && !session_exists
        && !msg.at_bot
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FeishuReplyKind {
    Text,
    Thinking,
    ToolCall,
    ToolResult,
    SubSession,
    Supervisor,
    Stats,
    Error,
}

impl FeishuReplyKind {
    fn is_standalone(self) -> bool {
        matches!(self, Self::SubSession | Self::Stats | Self::Error)
    }

    fn starts_new_message(self, active: Option<Self>) -> bool {
        match (self, active) {
            (Self::ToolResult, Some(Self::ToolCall)) => false,
            (Self::ToolCall, Some(Self::ToolCall)) => true,
            _ => active != Some(self) || self.is_standalone(),
        }
    }

    fn finishes_message(self) -> bool {
        matches!(
            self,
            Self::ToolResult | Self::SubSession | Self::Stats | Self::Error
        )
    }
}

struct FeishuReplyStream {
    gateway: FeishuGateway,
    parent_message_id: String,
    active_kind: Option<FeishuReplyKind>,
    active_card: Option<StreamingCard>,
    tool_cards: HashMap<String, StreamingCard>,
    compaction_cards: HashMap<String, StreamingCard>,
}

impl FeishuReplyStream {
    fn new(gateway: FeishuGateway, parent_message_id: String) -> Self {
        Self {
            gateway,
            parent_message_id,
            active_kind: None,
            active_card: None,
            tool_cards: HashMap::new(),
            compaction_cards: HashMap::new(),
        }
    }

    async fn push(&mut self, kind: FeishuReplyKind, chunk: &str) {
        if kind.starts_new_message(self.active_kind) {
            self.finish_active().await;
            self.active_card = Some(self.gateway.begin_streaming_reply(&self.parent_message_id));
            self.active_kind = Some(kind);
        }

        if let Some(card) = self.active_card.as_mut() {
            card.push(chunk).await.ok();
        }

        if kind.finishes_message() {
            self.finish_active().await;
        }
    }

    async fn finish(&mut self) {
        self.finish_active().await;
        for (_, mut card) in self.tool_cards.drain() {
            card.finish().await.ok();
        }
        for (_, mut card) in self.compaction_cards.drain() {
            card.finish().await.ok();
        }
    }

    async fn finish_active(&mut self) {
        if let Some(mut card) = self.active_card.take() {
            card.finish().await.ok();
        }
        self.active_kind = None;
    }

    async fn update_tool(&mut self, call_id: &str, line: &str, done: bool) {
        self.finish_active().await;
        let gateway = self.gateway.clone();
        let parent_message_id = self.parent_message_id.clone();
        let card = self
            .tool_cards
            .entry(call_id.to_string())
            .or_insert_with(|| gateway.begin_streaming_reply(&parent_message_id));
        if done {
            card.replace_final(line).await.ok();
            self.tool_cards.remove(call_id);
        } else {
            card.replace(line).await.ok();
        }
    }

    async fn update_context_compaction(&mut self, id: &str, line: &str, done: bool) {
        self.finish_active().await;
        let gateway = self.gateway.clone();
        let parent_message_id = self.parent_message_id.clone();
        let card = self
            .compaction_cards
            .entry(id.to_string())
            .or_insert_with(|| gateway.begin_streaming_reply(&parent_message_id));
        if done {
            card.replace_final(line).await.ok();
            self.compaction_cards.remove(id);
        } else {
            card.replace(line).await.ok();
        }
    }
}

async fn append_reply_chunk(
    output: &mut String,
    replies: &mut Option<&mut FeishuReplyStream>,
    kind: FeishuReplyKind,
    chunk: &str,
) {
    output.push_str(chunk);
    if let Some(replies) = replies.as_deref_mut() {
        replies.push(kind, chunk).await;
    }
}

fn fenced_block(lang: &str, content: &str) -> String {
    let sanitized = content.replace("```", "'''");
    format!("```{lang}\n{}\n```", sanitized.trim())
}

async fn update_tool_reply(
    output: &mut String,
    replies: &mut Option<&mut FeishuReplyStream>,
    call_id: &str,
    line: &str,
    done: bool,
) {
    if let Some(replies) = replies.as_deref_mut() {
        replies.update_tool(call_id, line, done).await;
        if done {
            output.push_str(line);
            output.push('\n');
        }
    } else {
        output.push_str(line);
        output.push('\n');
    }
}

async fn update_context_compaction_reply(
    output: &mut String,
    replies: &mut Option<&mut FeishuReplyStream>,
    id: &str,
    line: &str,
    done: bool,
) {
    if let Some(replies) = replies.as_deref_mut() {
        replies.update_context_compaction(id, line, done).await;
        if done {
            output.push_str(line);
            output.push('\n');
        }
    } else {
        output.push_str(line);
        output.push('\n');
    }
}

fn format_context_compaction_line(event: &bot_core::ContextCompactionEvent) -> String {
    match event.status {
        bot_core::ContextCompactionStatus::Started => format!(
            "🧠 正在压缩上下文：压缩 {} 条，保留 {} 条",
            event.compacted_messages, event.remaining_messages
        ),
        bot_core::ContextCompactionStatus::Completed => format!(
            "🧠 上下文已压缩：压缩 {} 条，保留 {} 条",
            event.compacted_messages, event.remaining_messages
        ),
        bot_core::ContextCompactionStatus::Failed => format!(
            "🧠 上下文压缩失败：{}",
            event.error.as_deref().unwrap_or("unknown error")
        ),
    }
}

fn format_feishu_tool_line(pretty: &bot_core::PrettyToolCall) -> String {
    let (icon, status) = match pretty.status {
        bot_core::PrettyToolStatus::Running => ("⏳", "运行中"),
        bot_core::PrettyToolStatus::Success => ("✅", "成功"),
        bot_core::PrettyToolStatus::Error => ("❌", "失败"),
    };
    let elapsed = pretty
        .elapsed_ms
        .map(format_elapsed)
        .map(|value| format!(" · {value}"))
        .unwrap_or_default();
    truncate_tool_line(&format!(
        "{icon} {status} **{}** — {}{elapsed}",
        single_line(&pretty.title),
        single_line(&pretty.summary)
    ))
}

fn truncate_tool_line(line: &str) -> String {
    const MAX_CHARS: usize = 140;
    if line.chars().count() <= MAX_CHARS {
        return line.to_string();
    }
    let mut truncated = line.chars().take(MAX_CHARS).collect::<String>();
    truncated.push('…');
    truncated
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn format_supervisor_progress(event: &bot_core::SupervisorTraceEvent) -> String {
    match event {
        bot_core::SupervisorTraceEvent::Thinking { content } => {
            format!("\n**Thinking**\n{}\n", fenced_block("text", content))
        }
        bot_core::SupervisorTraceEvent::ToolCall { name, args } => {
            let json = serde_json::to_string_pretty(args).unwrap_or_default();
            format!("\n**Tool `{name}`**\n{}\n", fenced_block("json", &json))
        }
        bot_core::SupervisorTraceEvent::ToolResult { name, result } => format!(
            "\n**Tool result `{name}`**\n{}\n",
            fenced_block("text", result)
        ),
        bot_core::SupervisorTraceEvent::OutputDelta { content } => content.clone(),
        bot_core::SupervisorTraceEvent::Output { content } => {
            format!("\n**Output**\n{}\n", fenced_block("json", content))
        }
    }
}

fn supervisor_reply_kind(event: &bot_core::SupervisorTraceEvent) -> FeishuReplyKind {
    match event {
        bot_core::SupervisorTraceEvent::Thinking { .. } => FeishuReplyKind::Thinking,
        bot_core::SupervisorTraceEvent::ToolCall { .. } => FeishuReplyKind::ToolCall,
        bot_core::SupervisorTraceEvent::ToolResult { .. } => FeishuReplyKind::ToolResult,
        bot_core::SupervisorTraceEvent::OutputDelta { .. }
        | bot_core::SupervisorTraceEvent::Output { .. } => FeishuReplyKind::Supervisor,
    }
}

fn env_var_present(key: &str) -> bool {
    std::env::var(key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

async fn record_sub_session_event(
    runtime: &Runtime,
    parent_session_id: &str,
    platform: &str,
    msg: &FeishuMessage,
    event: &SubSessionEvent,
) {
    let kind = if event.agent_name == "acp" {
        SubSessionKind::Acp
    } else {
        SubSessionKind::Agent
    };
    let payload = format!("{:?}", event.payload);
    let status = if payload.starts_with("Done") {
        "done"
    } else if payload.starts_with("Error") {
        "error"
    } else {
        "running"
    };
    if let Err(err) = runtime.sessions.lock().await.upsert_sub_session(
        parent_session_id,
        &event.sub_thread_id.0,
        kind.clone(),
        &event.agent_name,
        event.title.clone(),
        status,
    ) {
        warn!("failed to record sub-session: {err:#}");
    }

    if !matches!(event.payload, SubSessionEventPayload::Start) {
        return;
    }
    if platform != FEISHU_CHANNEL {
        return;
    }
    let already_bound = runtime
        .sessions
        .lock()
        .await
        .sub_session_channel_binding(parent_session_id, &event.sub_thread_id.0)
        .is_some();
    if already_bound {
        return;
    }

    let binding = runtime
        .im_bridge
        .sub_session_binding_upsert(SubSessionBindingUpsertRequest {
            parent_session_id: parent_session_id.to_string(),
            sub_session_id: event.sub_thread_id.0.clone(),
            kind: if kind == SubSessionKind::Acp {
                "acp".to_string()
            } else {
                "agent".to_string()
            },
            target: event.agent_name.clone(),
            title: event.title.clone(),
            platform: platform.to_string(),
            parent_channel_id: msg.chat_id.clone(),
            parent_thread_id: None,
            actor_user_id: Some(msg.sender_user_id.clone()),
        })
        .await;
    match binding {
        Ok(Some(binding)) => {
            if let Err(err) = runtime.sessions.lock().await.bind_sub_session_channel(
                parent_session_id,
                &event.sub_thread_id.0,
                ChannelBinding {
                    platform: binding.platform,
                    channel_id: binding.channel_id,
                },
            ) {
                warn!("failed to persist sub-session IM binding: {err:#}");
            }
        }
        Ok(None) => {}
        Err(err) => warn!("failed to create sub-session IM binding: {err:#}"),
    }
}

async fn ensure_im_username(
    user_store: &UserStore,
    gateway: &FeishuGateway,
    user_uuid: &str,
    channel_user_id: &str,
) -> Option<String> {
    if let Some(username) = user_store.username(user_uuid) {
        return Some(username);
    }
    match gateway.get_user_name(channel_user_id).await {
        Ok(Some(username)) if !username.trim().is_empty() => {
            let username = username.trim().to_string();
            let _ = user_store.set_username_if_missing(user_uuid, &username);
            Some(username)
        }
        _ => None,
    }
}

fn build_message_content(
    text: &str,
    image_urls: &[String],
    had_images: bool,
    attachment_count: usize,
    document_count: usize,
) -> Content {
    let trimmed = text.trim();
    let valid_images: Vec<String> = image_urls
        .iter()
        .map(|url| url.trim())
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if !valid_images.is_empty() {
        let mut parts = Vec::new();
        if !trimmed.is_empty() {
            parts.push(ContentPart::text(trimmed.to_string()));
        }
        for data_url in valid_images {
            parts.push(ContentPart::image_url(data_url));
        }
        return Content::parts(parts);
    }
    if !trimmed.is_empty() {
        return Content::text(trimmed.to_string());
    }
    let fallback = match (had_images, attachment_count > 0, document_count > 0) {
        (true, _, _) => "[user sent image]",
        (false, true, true) => "[user sent attachment and document link]",
        (false, true, false) => "[user sent attachment]",
        (false, false, true) => "[user sent document link]",
        (false, false, false) => "[user sent an empty message]",
    };
    Content::text(fallback.to_string())
}

fn next_arg(args: &[String], index: usize) -> anyhow::Result<String> {
    args.get(index + 1)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{} requires a value", args[index]))
}

fn optional_arg(args: &[String], index: usize) -> Option<String> {
    args.get(index + 1)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod cli_tests {
    use super::{
        build_cargo_install_args, extract_first_url, extract_lark_cli_config_from_json,
        feedback_repo, feishu_doctor_message, feishu_session_channel_id, feishu_topic_channel_id,
        first_available_port, format_context_compaction_line, format_feishu_tool_line,
        github_new_issue_url, is_goal_set_command, normalize_release_tag, parse_command,
        parse_global_args, parse_goal_max_rounds, parse_release_version, percent_encode_query,
        prefix_short_config_entry, redact_known_secrets, run_streaming_command,
        run_streaming_command_with_stdin, should_ignore_unaddressed_topic_start, update_available,
        AppCommand, CliConfig, FeedbackCommand, FeishuCommand, FeishuDoctorStatus, FeishuReplyKind,
        GitHubIssueCreateRequest, ProfileCommand, UpdateCommand,
    };
    use crate::profile_command::{
        ProfileAgentCommand, ProfileWorkflowCommand, PROFILE_RUNTIME_ENV_KEYS,
    };
    use bot_core::{GoalMaxRounds, PrettyToolCall};
    use im_feishu::FeishuMessage;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn cli_subcommand_starts_interactive_mode() {
        let config = CliConfig::from_args(&args(&["cli"])).unwrap();
        assert!(config.enabled);
        assert!(!config.tui);
        assert_eq!(config.once, None);
        assert!(!config.pure_prompt);
    }

    #[test]
    fn tui_subcommand_starts_terminal_ui_mode() {
        let config = CliConfig::from_args(&args(&[
            "tui",
            "--session",
            "desk",
            "--user",
            "u1",
            "--name",
            "Alice",
        ]))
        .unwrap();
        assert!(config.enabled);
        assert!(config.tui);
        assert!(!config.resume);
        assert_eq!(config.resume_session_id, None);
        assert_eq!(config.channel_id, "desk");
        assert_eq!(config.user_id, "u1");
        assert_eq!(config.username, "Alice");
        assert_eq!(config.once, None);
    }

    #[test]
    fn tui_subcommand_accepts_resume_selector() {
        let config = CliConfig::from_args(&args(&["tui", "resume", "abc123"])).unwrap();
        assert!(config.enabled);
        assert!(config.tui);
        assert!(config.resume);
        assert_eq!(config.resume_session_id.as_deref(), Some("abc123"));
        assert_eq!(config.once, None);

        let config = CliConfig::from_args(&args(&["--resume", "desk-session"])).unwrap();
        assert!(config.enabled);
        assert!(config.tui);
        assert!(config.resume);
        assert_eq!(config.resume_session_id.as_deref(), Some("desk-session"));
    }

    #[test]
    fn tui_resume_accepts_missing_selector_for_menu() {
        let config = CliConfig::from_args(&args(&["tui", "resume"])).unwrap();
        assert!(config.enabled);
        assert!(config.tui);
        assert!(config.resume);
        assert_eq!(config.resume_session_id, None);

        let config = CliConfig::from_args(&args(&["--resume", "--user", "alice"])).unwrap();
        assert!(config.enabled);
        assert!(config.tui);
        assert!(config.resume);
        assert_eq!(config.resume_session_id, None);
        assert_eq!(config.user_id, "alice");
    }

    #[test]
    fn admin_command_serves_management_api_only() {
        let config = CliConfig::from_args(&args(&["admin"])).unwrap();
        assert!(config.admin_only);
        assert!(!config.enabled);
    }

    #[test]
    fn setup_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&["setup"])).unwrap(),
            AppCommand::Setup(entries) if entries.is_empty()
        ));
    }

    #[test]
    fn global_profile_is_removed_before_command_parsing() {
        let parsed = parse_global_args(&args(&["--profile", "dev", "doctor"])).unwrap();
        assert_eq!(parsed.profile.as_deref(), Some("dev"));
        assert_eq!(parsed.command_args, args(&["doctor"]));

        let parsed = parse_global_args(&args(&["doctor", "--profile=prod-2"])).unwrap();
        assert_eq!(parsed.profile.as_deref(), Some("prod-2"));
        assert_eq!(parsed.command_args, args(&["doctor"]));
    }

    #[test]
    fn rejects_invalid_or_duplicate_global_profiles() {
        assert!(parse_global_args(&args(&["--profile", "../dev", "doctor"])).is_err());
        assert!(
            parse_global_args(&args(&["--profile", "dev", "--profile", "prod", "doctor",]))
                .is_err()
        );
    }

    #[test]
    fn profile_management_commands_are_recognized() {
        assert!(matches!(
            parse_command(&args(&["profile", "list"])).unwrap(),
            AppCommand::Profile(ProfileCommand::List)
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "delete", "dev", "--force"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Delete { name, force })
                if name == "dev" && force
        ));
        assert!(parse_command(&args(&["profile", "delete", "default", "--force"])).is_err());
        assert!(matches!(
            parse_command(&args(&["profile", "create", "dev", "admin.port=8790"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Create { name, entries })
                if name == "dev" && entries == args(&["admin.port=8790"])
        ));
        assert!(parse_command(&args(&["profile", "create", "remi_diagnostics"])).is_err());
        assert!(
            parse_command(&args(&["profile", "delete", "remi_diagnostics", "--force"])).is_err()
        );
        assert!(matches!(
            parse_command(&args(&["profile", "start", "default"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Start(name)) if name == "default"
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "stop", "dev", "--force"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Stop { name, force }) if name == "dev" && force
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "restart", "dev"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Restart { name, force }) if name == "dev" && !force
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "status", "--all"])).unwrap(),
            AppCommand::Profile(ProfileCommand::StatusAll)
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "status", "default"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Status(name)) if name == "default"
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "agent", "list", "dev"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Agent(ProfileAgentCommand::List { profile }))
                if profile == "dev"
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "agent", "upsert", "dev", "/tmp/a.md"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Agent(ProfileAgentCommand::Upsert { profile, path }))
                if profile == "dev" && path == "/tmp/a.md"
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "agent", "set-default", "dev", "coder"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Agent(ProfileAgentCommand::SetDefault { profile, agent_id }))
                if profile == "dev" && agent_id == "coder"
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "workflow", "list", "dev"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Workflow(ProfileWorkflowCommand::List { profile }))
                if profile == "dev"
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "workflow", "show", "dev", "verify"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Workflow(ProfileWorkflowCommand::Show { profile, workflow_id }))
                if profile == "dev" && workflow_id == "verify"
        ));
        assert!(matches!(
            parse_command(&args(&["profile", "workflow", "delete", "dev", "verify"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Workflow(ProfileWorkflowCommand::Delete { profile, workflow_id }))
                if profile == "dev" && workflow_id == "verify"
        ));
        assert!(parse_command(&args(&["profile", "workflow", "delete", "dev", "goal"])).is_err());
        assert!(matches!(
            parse_command(&args(&["config", "set", "admin.port=8790"])).unwrap(),
            AppCommand::ConfigSet(entries) if entries == args(&["admin.port=8790"])
        ));
        assert!(matches!(
            parse_command(&args(&["config", "set", "shell.mode=local"])).unwrap(),
            AppCommand::ConfigSet(entries) if entries == args(&["shell.mode=local"])
        ));
        assert!(matches!(
            parse_command(&args(&["sandbox", "set", "kind=no_sandbox"])).unwrap(),
            AppCommand::SandboxSet(entries) if entries == args(&["kind=no_sandbox"])
        ));
    }

    #[test]
    fn profile_start_clears_runtime_override_env() {
        for key in [
            "REMI_AGENT_ID",
            "REMI_MODEL_PROFILE",
            "REMI_AGENTS_DIR",
            "REMI_SANDBOX_KIND",
            "REMI_SANDBOX_HOST_DIR",
            "REMI_ADMIN_PORT",
            "REMI_IM_MODE",
            "REMI_SHELL_MODE",
            "REMI_ACP_CLIENT",
        ] {
            assert!(
                PROFILE_RUNTIME_ENV_KEYS.contains(&key),
                "missing runtime env cleanup for {key}"
            );
        }
    }

    #[test]
    fn sandbox_short_entries_are_prefixed_by_key_only() {
        assert_eq!(
            prefix_short_config_entry("sandbox", "host_dir=.remi-cat/profiles/dev"),
            "sandbox.host_dir=.remi-cat/profiles/dev"
        );
        assert_eq!(
            prefix_short_config_entry("sandbox", "sandbox.kind=docker"),
            "sandbox.kind=docker"
        );
    }

    #[test]
    fn occupied_setup_port_moves_upward() {
        let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", 0)) else {
            return;
        };
        let occupied = listener.local_addr().unwrap().port();
        if occupied < u16::MAX {
            let selected =
                first_available_port("127.0.0.1", occupied, &std::collections::HashSet::new())
                    .unwrap();
            assert!(selected > occupied);
        }
    }

    #[test]
    fn parses_goal_max_rounds() {
        assert_eq!(
            parse_goal_max_rounds("20").unwrap(),
            GoalMaxRounds::Limited(20)
        );
        assert_eq!(
            parse_goal_max_rounds("unlimited").unwrap(),
            GoalMaxRounds::Unlimited
        );
        assert!(parse_goal_max_rounds("0").is_err());
    }

    #[test]
    fn recognizes_goal_set_command_without_matching_status() {
        assert!(is_goal_set_command("/goal set 分析深圳房价"));
        assert!(is_goal_set_command(" /goal set --max-rounds 3 test "));
        assert!(!is_goal_set_command("/goal status"));
        assert!(!is_goal_set_command("/goal setting"));
    }

    #[test]
    fn doctor_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&["doctor"])).unwrap(),
            AppCommand::Doctor
        ));
    }

    #[test]
    fn feishu_init_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&["feishu", "init"])).unwrap(),
            AppCommand::Feishu(FeishuCommand::Init)
        ));
    }

    #[test]
    fn feishu_doctor_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&["feishu", "doctor"])).unwrap(),
            AppCommand::Feishu(FeishuCommand::Doctor)
        ));
    }

    #[test]
    fn update_check_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&["update", "check"])).unwrap(),
            AppCommand::Update(UpdateCommand::Check { json: false })
        ));
        assert!(matches!(
            parse_command(&args(&["update", "check", "--json"])).unwrap(),
            AppCommand::Update(UpdateCommand::Check { json: true })
        ));
    }

    #[test]
    fn update_self_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&[
                "update",
                "self",
                "--version",
                "v0.2.1",
                "--dry-run",
                "--force",
            ]))
            .unwrap(),
            AppCommand::Update(UpdateCommand::SelfUpdate { version, force: true, dry_run: true })
                if version.as_deref() == Some("v0.2.1")
        ));
        assert!(matches!(
            parse_command(&args(&["update", "self", "--version=0.2.1"])).unwrap(),
            AppCommand::Update(UpdateCommand::SelfUpdate { version, force: false, dry_run: false })
                if version.as_deref() == Some("0.2.1")
        ));
    }

    #[test]
    fn update_version_helpers_normalize_tags_and_compare_versions() {
        assert_eq!(normalize_release_tag("0.2.1").unwrap(), "v0.2.1");
        assert_eq!(normalize_release_tag("v0.2.1").unwrap(), "v0.2.1");
        assert!(parse_release_version("not-a-version").is_err());
        assert!(update_available("0.2.0", "0.2.1").unwrap());
        assert!(!update_available("0.2.1", "0.2.1").unwrap());
        assert!(!update_available("0.3.0", "0.2.1").unwrap());
        assert!(update_available("0.2.0-alpha.1", "0.2.0").unwrap());
    }

    #[test]
    fn update_self_builds_cargo_install_args() {
        assert_eq!(
            build_cargo_install_args("https://github.com/another-s347/remi-cat.git", "v0.2.1"),
            args(&[
                "install",
                "--git",
                "https://github.com/another-s347/remi-cat.git",
                "--tag",
                "v0.2.1",
                "remi-cat",
                "--locked",
                "--force",
            ])
        );
    }

    #[test]
    fn feedback_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&[
                "feedback",
                "--title",
                "Bash timeout is confusing",
                "--body",
                "The command kept running.",
                "--label",
                "bug,cli",
                "--include-logs",
                "--dry-run",
            ]))
            .unwrap(),
            AppCommand::Feedback(FeedbackCommand {
                title,
                body,
                labels,
                include_logs: true,
                dry_run: true,
                ..
            }) if title == "Bash timeout is confusing"
                && body == "The command kept running."
                && labels == args(&["bug", "cli", "feedback"])
        ));
    }

    #[test]
    fn issue_create_command_reuses_feedback_flow() {
        assert!(matches!(
            parse_command(&args(&[
                "issue",
                "create",
                "--title=Install fails",
                "--repo",
                "owner/project",
                "--no-default-label",
            ]))
            .unwrap(),
            AppCommand::Feedback(FeedbackCommand { title, repo, labels, .. })
                if title == "Install fails"
                    && repo.as_deref() == Some("owner/project")
                    && labels.is_empty()
        ));
    }

    #[test]
    fn feedback_positional_message_becomes_title_and_body() {
        assert!(matches!(
            parse_command(&args(&["feedback", "short", "message"])).unwrap(),
            AppCommand::Feedback(FeedbackCommand { title, body, .. })
                if title == "short message" && body == "short message"
        ));
    }

    #[test]
    fn github_issue_url_is_prefilled_and_encoded() {
        let payload = GitHubIssueCreateRequest {
            title: "A bug & a fix".to_string(),
            body: "line 1\nline 2".to_string(),
            labels: args(&["feedback", "bug"]),
        };

        let url = github_new_issue_url("owner/repo", &payload);

        assert!(url.starts_with("https://github.com/owner/repo/issues/new?"));
        assert!(url.contains("title=A+bug+%26+a+fix"));
        assert!(url.contains("body=line+1%0Aline+2"));
        assert!(url.contains("labels=feedback%2Cbug"));
        assert_eq!(percent_encode_query("你好"), "%E4%BD%A0%E5%A5%BD");
    }

    #[test]
    fn feedback_repo_validates_owner_repo() {
        assert_eq!(feedback_repo(Some("owner/repo")).unwrap(), "owner/repo");
        assert!(feedback_repo(Some("owner")).is_err());
        assert!(feedback_repo(Some("owner/repo/extra")).is_err());
    }

    #[test]
    fn feedback_log_redaction_replaces_known_secret_values() {
        let redactions = std::collections::HashMap::from([(
            "GITHUB_TOKEN".to_string(),
            "ghp_secret".to_string(),
        )]);

        assert_eq!(
            redact_known_secrets("token=ghp_secret", &redactions),
            "token=***REDACTED***"
        );
    }

    #[test]
    fn feishu_reply_events_pair_tool_request_and_response() {
        assert!(!FeishuReplyKind::Text.starts_new_message(Some(FeishuReplyKind::Text)));
        assert!(FeishuReplyKind::Thinking.starts_new_message(Some(FeishuReplyKind::Text)));
        assert!(FeishuReplyKind::ToolCall.starts_new_message(Some(FeishuReplyKind::ToolCall)));
        assert!(!FeishuReplyKind::ToolResult.starts_new_message(Some(FeishuReplyKind::ToolCall)));
        assert!(FeishuReplyKind::ToolResult.starts_new_message(None));
        assert!(FeishuReplyKind::ToolResult.finishes_message());
        assert!(FeishuReplyKind::Text.starts_new_message(None));
    }

    #[test]
    fn formats_context_compaction_for_feishu() {
        let mut event = bot_core::ContextCompactionEvent {
            id: "compact-1".to_string(),
            thread_id: "thread-1".to_string(),
            status: bot_core::ContextCompactionStatus::Started,
            source: bot_core::ContextCompactionSource::Auto,
            compacted_messages: 5,
            remaining_messages: 3,
            error: None,
        };
        assert!(format_context_compaction_line(&event).contains("正在压缩上下文"));
        event.status = bot_core::ContextCompactionStatus::Completed;
        assert!(format_context_compaction_line(&event).contains("上下文已压缩"));
        event.status = bot_core::ContextCompactionStatus::Failed;
        event.error = Some("model unavailable".to_string());
        assert!(format_context_compaction_line(&event).contains("model unavailable"));
    }

    #[test]
    fn feishu_tool_pretty_line_is_compact() {
        let pretty = PrettyToolCall::completed(
            "call-1",
            "search",
            &serde_json::json!({"query": "exa"}),
            "found result\nwith details",
            true,
            1234,
        );

        let line = format_feishu_tool_line(&pretty);

        assert!(line.starts_with("✅ 成功 "));
        assert!(line.contains(" · 1.2s"));
        assert!(!line.contains("<details>"));
        assert!(!line.contains("完整 request"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn feishu_tool_pretty_line_truncates_long_summary() {
        let pretty = PrettyToolCall::completed(
            "call-1",
            "manage_yourself",
            &serde_json::json!({"command": "profile agent list default"}),
            "default\tRemi\tdefault\tsearch, skill__get, skill__read_resource, todo__add, todo__list, todo__complete, todo__update, todo__remove, trigger__upsert, trigger__list, trigger__delete, memory__upsert_named, memory__get_detail, bash, fs_read, fs_write, apply_patch, fs_mkdir, fs_remove, fs_ls, fetch, acp__chat, manage_yourself",
            true,
            511,
        );

        let line = format_feishu_tool_line(&pretty);

        assert!(line.chars().count() <= 141);
        assert!(line.contains("default"));
        assert!(!line.contains("skill__read_resource"));
    }

    #[test]
    fn cli_subcommand_accepts_message_tail() {
        let config = CliConfig::from_args(&args(&["cli", "hello", "from", "cli"])).unwrap();
        assert_eq!(config.once.as_deref(), Some("hello from cli"));
    }

    #[test]
    fn cli_subcommand_allows_flags_before_message() {
        let config = CliConfig::from_args(&args(&[
            "cli",
            "--channel",
            "support",
            "--user",
            "alice",
            "-m",
            "/tools",
        ]))
        .unwrap();
        assert_eq!(config.channel_id, "support");
        assert_eq!(config.user_id, "alice");
        assert_eq!(config.once.as_deref(), Some("/tools"));
    }

    #[test]
    fn cli_message_preserves_remaining_words_and_channel() {
        let config = CliConfig::from_args(&args(&[
            "--channel",
            "support",
            "--user",
            "alice",
            "--name",
            "Alice",
            "-m",
            "hello",
            "world",
        ]))
        .unwrap();
        assert!(config.enabled);
        assert_eq!(config.channel_id, "support");
        assert_eq!(config.user_id, "alice");
        assert_eq!(config.username, "Alice");
        assert_eq!(config.once.as_deref(), Some("hello world"));
    }

    #[test]
    fn prompt_flag_accepts_prompt_tail_and_session() {
        let config =
            CliConfig::from_args(&args(&["-p", "--session", "lme-demo", "hello", "world"]))
                .unwrap();
        assert!(config.enabled);
        assert!(config.pure_prompt);
        assert_eq!(config.channel_id, "lme-demo");
        assert_eq!(config.once.as_deref(), Some("hello world"));
    }

    #[test]
    fn prompt_subcommand_accepts_message_tail() {
        let config = CliConfig::from_args(&args(&["prompt", "--session", "s1", "hello"])).unwrap();
        assert!(config.enabled);
        assert!(config.pure_prompt);
        assert_eq!(config.channel_id, "s1");
        assert_eq!(config.once.as_deref(), Some("hello"));
    }

    #[test]
    fn topic_group_message_uses_thread_scoped_session_channel() {
        let msg = FeishuMessage {
            message_id: "om_msg".to_string(),
            sender_user_id: "ou_user".to_string(),
            chat_id: "oc_chat".to_string(),
            chat_type: "group".to_string(),
            text: "hello".to_string(),
            images: Vec::new(),
            files: Vec::new(),
            documents: Vec::new(),
            parent_id: None,
            thread_id: Some("omt_topic".to_string()),
            at_bot: true,
            mentions: Vec::new(),
        };
        assert_eq!(feishu_session_channel_id(&msg), "oc_chat:thread:omt_topic");
    }

    #[test]
    fn feishu_topic_channel_id_matches_session_channel_format() {
        assert_eq!(
            feishu_topic_channel_id("oc_chat", "omt_fork"),
            "oc_chat:thread:omt_fork"
        );
    }

    #[test]
    fn non_topic_group_message_uses_chat_session_channel() {
        let msg = FeishuMessage {
            message_id: "om_msg".to_string(),
            sender_user_id: "ou_user".to_string(),
            chat_id: "oc_chat".to_string(),
            chat_type: "group".to_string(),
            text: "hello".to_string(),
            images: Vec::new(),
            files: Vec::new(),
            documents: Vec::new(),
            parent_id: Some("om_parent".to_string()),
            thread_id: None,
            at_bot: true,
            mentions: Vec::new(),
        };
        assert_eq!(feishu_session_channel_id(&msg), "oc_chat");
    }

    #[test]
    fn unaddressed_topic_start_is_ignored_until_session_exists() {
        let msg = FeishuMessage {
            message_id: "om_msg".to_string(),
            sender_user_id: "ou_user".to_string(),
            chat_id: "oc_chat".to_string(),
            chat_type: "group".to_string(),
            text: "hello".to_string(),
            images: Vec::new(),
            files: Vec::new(),
            documents: Vec::new(),
            parent_id: None,
            thread_id: Some("omt_topic".to_string()),
            at_bot: false,
            mentions: Vec::new(),
        };
        assert!(should_ignore_unaddressed_topic_start(&msg, false));
        assert!(!should_ignore_unaddressed_topic_start(&msg, true));
    }

    #[test]
    fn addressed_topic_start_is_processed() {
        let msg = FeishuMessage {
            message_id: "om_msg".to_string(),
            sender_user_id: "ou_user".to_string(),
            chat_id: "oc_chat".to_string(),
            chat_type: "group".to_string(),
            text: "hello".to_string(),
            images: Vec::new(),
            files: Vec::new(),
            documents: Vec::new(),
            parent_id: None,
            thread_id: Some("omt_topic".to_string()),
            at_bot: true,
            mentions: Vec::new(),
        };
        assert!(!should_ignore_unaddressed_topic_start(&msg, false));
    }

    #[test]
    fn unaddressed_non_topic_group_keeps_existing_behavior() {
        let msg = FeishuMessage {
            message_id: "om_msg".to_string(),
            sender_user_id: "ou_user".to_string(),
            chat_id: "oc_chat".to_string(),
            chat_type: "group".to_string(),
            text: "hello".to_string(),
            images: Vec::new(),
            files: Vec::new(),
            documents: Vec::new(),
            parent_id: None,
            thread_id: None,
            at_bot: false,
            mentions: Vec::new(),
        };
        assert!(!should_ignore_unaddressed_topic_start(&msg, false));
    }

    #[test]
    fn extracts_first_url_from_cli_output() {
        let url = extract_first_url("Continue in browser: https://open.feishu.cn/device/abc123).");
        assert_eq!(url.as_deref(), Some("https://open.feishu.cn/device/abc123"));
    }

    #[test]
    fn parses_lark_cli_config_snapshot_from_apps_array() {
        let json = serde_json::json!({
            "apps": [
                { "appId": "cli_123", "appSecret": "secret_456" }
            ]
        });
        let snapshot = extract_lark_cli_config_from_json(&json);
        assert_eq!(snapshot.app_id.as_deref(), Some("cli_123"));
        assert_eq!(snapshot.app_secret.as_deref(), Some("secret_456"));
    }

    #[test]
    fn doctor_message_highlights_logged_in_but_missing_remi_env() {
        let status = FeishuDoctorStatus {
            lark_cli_installed: true,
            lark_cli_config: None,
            auth_status: Some(super::AuthStatusSummary {
                success: true,
                output: "ok".to_string(),
            }),
            remi_app_id_present: false,
            remi_app_secret_present: false,
        };
        assert!(feishu_doctor_message(&status)
            .unwrap()
            .contains("lark-cli is logged in"));
    }

    #[tokio::test]
    async fn streaming_command_captures_url_and_success() {
        let script = write_mock_cli(
            "config",
            r#"#!/bin/sh
if [ "$1" = "config" ] && [ "$2" = "init" ]; then
  echo "Open https://open.feishu.cn/device/mock-config now"
  exit 0
fi
exit 1
"#,
        );

        let result = run_streaming_command(
            "sh",
            &[script.to_str().unwrap(), "config", "init", "--new"],
            "hint",
        )
        .await
        .unwrap();

        assert!(result.success);
        assert_eq!(
            result.first_url.as_deref(),
            Some("https://open.feishu.cn/device/mock-config")
        );
    }

    #[tokio::test]
    async fn streaming_command_reports_failure_without_url() {
        let script = write_mock_cli(
            "login",
            r#"#!/bin/sh
echo "login failed" >&2
exit 1
"#,
        );

        let result = run_streaming_command(
            "sh",
            &[script.to_str().unwrap(), "auth", "login", "--recommend"],
            "hint",
        )
        .await
        .unwrap();

        assert!(!result.success);
        assert!(result.first_url.is_none());
        assert!(result
            .lines
            .iter()
            .any(|line| line.contains("login failed")));
    }

    #[tokio::test]
    async fn streaming_command_can_write_stdin() {
        let script = write_mock_cli(
            "stdin",
            r#"#!/bin/sh
read secret
if [ "$secret" = "expected-secret" ]; then
  echo "secret received"
  exit 0
fi
exit 1
"#,
        );

        let result = run_streaming_command_with_stdin(
            "sh",
            &[script.to_str().unwrap()],
            "hint",
            Some("expected-secret\n"),
        )
        .await
        .unwrap();

        assert!(result.success);
        assert!(result.lines.iter().any(|line| line == "secret received"));
    }

    fn write_mock_cli(name: &str, body: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("remi-cat-{name}-{}.sh", uuid::Uuid::new_v4()));
        fs::write(&path, body).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }
}
