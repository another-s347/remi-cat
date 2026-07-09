use anyhow::Context;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::channel::cli::{process_cli_message, process_prompt_message};
use crate::channel::feishu::im_mode_from_env;
#[cfg(test)]
pub(crate) use crate::channel::feishu::{
    feishu_session_channel_id, feishu_topic_channel_id, format_context_compaction_line,
    format_feishu_tool_line, should_ignore_unaddressed_topic_start, FeishuReplyKind,
};
use crate::channel::Channel;
#[cfg(test)]
pub(crate) use crate::cli::UpdateCommand;
pub(crate) use crate::cli::{
    parse_cli_args, try_parse_cli_args, AcpAdapterCommand, AcpCommand, AppCommand, CliConfig,
    FeishuCommand, TasksCommand, CLI_USER_ID,
};
#[cfg(test)]
pub(crate) use crate::cli::{parse_command, parse_global_args};
#[cfg(test)]
pub(crate) use crate::cli::{
    CodexCommand, FeedbackCommand, GitHubIssueCreateRequest, HooksCommand,
};
#[cfg(test)]
pub(crate) use crate::command::{
    build_cargo_install_args, feedback_repo, github_new_issue_url, normalize_release_tag,
    parse_release_version, percent_encode_query, redact_known_secrets, update_available,
};
#[cfg(test)]
pub(crate) use crate::command::{
    direct_workflow_options, is_goal_set_command, parse_goal_max_rounds,
    parse_workflow_start_options,
};
#[cfg(test)]
pub(crate) use crate::command::{
    extract_first_url, extract_lark_cli_config_from_json, feishu_doctor_message,
    run_streaming_command, run_streaming_command_with_stdin, AuthStatusSummary, FeishuDoctorStatus,
};
pub(crate) use crate::command::{
    print_registered_tools, run_acp_command, run_codex_command, run_doctor, run_feedback_command,
    run_feishu_doctor, run_feishu_init, run_hooks_command, run_secret_command, run_setup,
    run_update_command,
};
use crate::config::{
    detect_setup_state, has_legacy_env_credentials, resolve_runtime_config_for_run, ImMode,
    SetupState,
};
use crate::core::Runtime;
use crate::instance_profile::{tui_home_data_dir, InstanceProfile, DIAGNOSTIC_PROFILE_NAME};
#[cfg(test)]
pub(crate) use crate::profile_command::first_available_port;
#[cfg(test)]
pub(crate) use crate::profile_command::ProfileCommand;
use crate::profile_command::{
    apply_runtime_config_entries, ensure_builtin_diagnostic_profile, prefix_short_config_entry,
    run_noninteractive_setup, run_profile_command,
};
use crate::secret_store::{apply_entries_to_env, redaction_entries, SecretStore};
use crate::session::SessionRuntime;
use crate::{host_admin, web_chat};
use bot_core::im_tools::ImFileBridge;
use bot_core::{install_embedded_agent_profiles, install_embedded_model_profiles, CatBotBuilder};
use im_feishu::FeishuGateway;
use tokio::sync::Mutex;
use tracing::{info, warn};
use user_store::UserStore;

pub(crate) const FEISHU_CHANNEL: &str = "feishu";
pub(crate) const CLI_CHANNEL: &str = "cli";
pub(crate) const CLI_CHAT_ID: &str = "local-dev";
pub(crate) const CLI_USERNAME: &str = "local-user";
pub(crate) const SESSION_DEBUG_METADATA_KEY: &str = "debug";
pub(crate) const SESSION_MODEL_PROFILE_METADATA_KEY: &str = "model_profile_id";
pub(crate) const SESSION_REASONING_EFFORT_METADATA_KEY: &str = "reasoning_effort";
pub(crate) const SESSION_AGENT_ID_METADATA_KEY: &str = "agent_id";
pub(crate) const SESSION_INPUT_HISTORY_METADATA_KEY: &str = "input_history";
pub(crate) const MAX_COMMAND_PREPROCESS_DEPTH: usize = 8;

pub(crate) fn parse_session_reasoning_effort(
    raw: Option<String>,
) -> Option<bot_core::ReasoningEffort> {
    raw.as_deref()
        .and_then(bot_core::ReasoningEffort::parse)
        .filter(|effort| *effort != bot_core::ReasoningEffort::Auto)
}

fn apply_runtime_env_defaults(data_dir: &mut PathBuf, tui_mode: bool) {
    match resolve_runtime_config_for_run(data_dir, tui_mode) {
        Ok(resolution) => {
            tracing::info!(
                source = ?resolution.source,
                model_profile = resolution
                    .config
                    .as_ref()
                    .map(|config| config.model_profile.as_str())
                    .unwrap_or(""),
                data_dir = %resolution.data_dir.display(),
                "resolved runtime config"
            );
            *data_dir = resolution.data_dir;
        }
        Err(err) => tracing::warn!(error = %err, "failed to resolve runtime config"),
    }
}

pub(crate) async fn run() -> anyhow::Result<()> {
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match parse_cli_args(&raw_args) {
        Ok(parsed) => parsed,
        Err(err) => match try_parse_cli_args(&raw_args) {
            Ok(_) => return Err(err),
            Err(clap_err) => clap_err.exit(),
        },
    };
    let _ = dotenvy::dotenv();
    let tool_output_overflow_bytes = parsed.tool_output_overflow_bytes;
    let command = parsed.command;
    let tui_mode = matches!(&command, AppCommand::Run(cli) if cli.tui);
    let acp_agent_mode = matches!(&command, AppCommand::Acp(AcpCommand::Agent));
    let explicit_data_dir = if tui_mode {
        None
    } else if acp_agent_mode {
        absolute_env_path("REMI_DATA_DIR")
    } else {
        std::env::var_os("REMI_DATA_DIR").map(PathBuf::from)
    };
    let selected_profile = resolve_instance_profile(
        parsed.profile,
        explicit_data_dir.clone(),
        tui_mode,
        acp_agent_mode,
    )?;
    if selected_profile.label() == DIAGNOSTIC_PROFILE_NAME {
        ensure_builtin_diagnostic_profile()?;
    }
    let mut data_dir = selected_profile.data_dir.clone();
    std::fs::create_dir_all(&data_dir)?;
    let secret_store = Arc::new(Mutex::new(if tui_mode || acp_agent_mode {
        SecretStore::from_env_with_default_dotenv_path(data_dir.join(".env"))
    } else {
        SecretStore::from_env()
    }));
    let secret_backend_label = secret_store.lock().await.backend_label();
    let startup_secrets = secret_store.lock().await.entries()?;
    apply_entries_to_env(&startup_secrets);
    let _observability_guard = init_observability(
        matches!(
            &command,
            AppCommand::Run(cli) if cli.tui
        ),
        acp_agent_mode,
        &data_dir,
    )?;
    tracing::info!(backend = secret_backend_label, "secret store initialized");
    tracing::info!(
        tui_mode,
        acp_agent_mode,
        selected_profile = selected_profile.label(),
        data_dir = %data_dir.display(),
        explicit_data_dir = explicit_data_dir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        env_model_profile = std::env::var("REMI_MODEL_PROFILE").unwrap_or_default(),
        "remi startup profile resolved"
    );

    if let AppCommand::Profile(profile_command) = &command {
        run_profile_command(profile_command, &data_dir).await?;
        return Ok(());
    }

    match command {
        AppCommand::Setup(entries) => {
            if entries.is_empty() {
                run_setup(&selected_profile, &mut data_dir, Arc::clone(&secret_store)).await?;
            } else {
                run_noninteractive_setup(&selected_profile, &data_dir, &entries).await?;
            }
            return Ok(());
        }
        AppCommand::Doctor => {
            run_doctor(&selected_profile, &data_dir)?;
            return Ok(());
        }
        AppCommand::Hooks(command) => {
            run_hooks_command(&selected_profile, &data_dir, command).await?;
            return Ok(());
        }
        AppCommand::Tasks(command) => {
            run_tasks_command(&data_dir, &command).await?;
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
            apply_runtime_config_entries(&selected_profile, &data_dir, &entries, false).await?;
            return Ok(());
        }
        AppCommand::SandboxSet(entries) => {
            let entries = entries
                .iter()
                .map(|entry| prefix_short_config_entry("sandbox", entry))
                .collect::<Vec<_>>();
            apply_runtime_config_entries(&selected_profile, &data_dir, &entries, false).await?;
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
        AppCommand::AcpAdapter(AcpAdapterCommand::Codex { bin, args }) => {
            return crate::codex_acp_adapter::run_codex_adapter(bin, args).await;
        }
        AppCommand::Acp(AcpCommand::Agent) => {
            unsafe {
                std::env::set_var("REMI_DATA_DIR", &data_dir);
            }
            apply_runtime_env_defaults(&mut data_dir, false);
            tracing::info!(
                data_dir = %data_dir.display(),
                env_data_dir = std::env::var("REMI_DATA_DIR").unwrap_or_default(),
                env_agent_id = std::env::var("REMI_AGENT_ID").unwrap_or_default(),
                env_model_profile = std::env::var("REMI_MODEL_PROFILE").unwrap_or_default(),
                env_sandbox_kind = std::env::var("REMI_SANDBOX_KIND").unwrap_or_default(),
                "ACP agent runtime env applied"
            );
            if let Some(overflow_bytes) = tool_output_overflow_bytes {
                unsafe {
                    std::env::set_var(
                        "REMI_TOOL_OUTPUT_OVERFLOW_BYTES",
                        overflow_bytes.to_string(),
                    );
                }
            }
            if !matches!(
                detect_setup_state(&data_dir),
                SetupState::Initialized { .. }
            ) && !has_legacy_env_credentials()
            {
                anyhow::bail!(
                    "remi-cat is not initialized yet. Run `remi-cat setup` first, or provide legacy env config."
                );
            }
        }
        AppCommand::Acp(command) => {
            run_acp_command(&selected_profile, &data_dir, command).await?;
            return Ok(());
        }
        AppCommand::Codex(command) => {
            run_codex_command(&selected_profile, &data_dir, command).await?;
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
            apply_runtime_env_defaults(&mut data_dir, cli.tui);
            if cli.tui {
                let workspace_dir =
                    std::env::current_dir().context("resolving current workspace")?;
                unsafe {
                    std::env::set_var("REMI_SANDBOX_HOST_DIR", &workspace_dir);
                }
            }
            if let Some(overflow_bytes) = tool_output_overflow_bytes {
                unsafe {
                    std::env::set_var(
                        "REMI_TOOL_OUTPUT_OVERFLOW_BYTES",
                        overflow_bytes.to_string(),
                    );
                }
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
        AppCommand::Tools(_) => {
            unsafe {
                std::env::set_var("REMI_DATA_DIR", &data_dir);
            }
            if let Ok(resolution) = resolve_runtime_config_for_run(&data_dir, false) {
                tracing::debug!(
                    source = ?resolution.source,
                    model_profile = resolution
                        .config
                        .as_ref()
                        .map(|config| config.model_profile.as_str())
                        .unwrap_or(""),
                    data_dir = %resolution.data_dir.display(),
                    "resolved runtime config"
                );
                data_dir = resolution.data_dir;
            }
            if let Some(overflow_bytes) = tool_output_overflow_bytes {
                unsafe {
                    std::env::set_var(
                        "REMI_TOOL_OUTPUT_OVERFLOW_BYTES",
                        overflow_bytes.to_string(),
                    );
                }
            }
            if !matches!(
                detect_setup_state(&data_dir),
                SetupState::Initialized { .. }
            ) && !has_legacy_env_credentials()
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

    if matches!(&command, AppCommand::Acp(AcpCommand::Agent)) {
        let root_agent_id =
            std::env::var("REMI_AGENT_ID").unwrap_or_else(|_| "default".to_string());
        let sessions = Arc::new(Mutex::new(SessionRuntime::load(data_dir.clone())?));
        let factory = Rc::new(crate::acp_agent::AcpRuntimeFactory::new(
            data_dir.clone(),
            secret_store,
            sessions,
            root_agent_id,
        ));
        return crate::acp_agent::run_stdio_agent(factory).await;
    }

    let cli = match &command {
        AppCommand::Run(cli) => cli.clone(),
        AppCommand::Tools(_) | AppCommand::Tasks(_) => CliConfig {
            enabled: false,
            tui: false,
            resume: false,
            resume_session_id: None,
            once: None,
            pure_prompt: false,
            admin_only: false,
            channel_id: CLI_CHAT_ID.to_string(),
            user_id: CLI_USER_ID.to_string(),
            username: CLI_USERNAME.to_string(),
            wait_background_tasks: false,
        },
        _ => unreachable!(),
    };
    let root_agent_id = std::env::var("REMI_AGENT_ID").unwrap_or_else(|_| "default".to_string());
    let agents_dir = std::env::var("REMI_AGENTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("agents"));

    if cli.admin_only {
        let workspace_dir = current_workspace_dir(&data_dir);
        let sessions = Arc::new(Mutex::new(SessionRuntime::load(data_dir.clone())?));
        maybe_start_admin(host_admin::AdminState {
            agents_dir,
            skills_dir: data_dir.join("skills"),
            workspace_root_label: current_workspace_root_label(&workspace_dir),
            workspace_dir,
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
                warn!("Feishu credentials are not configured — running in Web-only mode");
                None
            }
        }
    };

    let bridge: Arc<dyn ImFileBridge> = Arc::new(crate::channel::feishu::LocalImFileBridge::new(
        gateway.clone(),
    ));
    let bot = Rc::new(
        CatBotBuilder::from_env()?
            .im_bridge(Arc::clone(&bridge))
            .build()?,
    );
    bot.update_secret_redactor(&redaction_entries(&secret_store.lock().await.entries()?));
    if let AppCommand::Tools(args) = &command {
        print_registered_tools(&bot, args.json)?;
        return Ok(());
    }
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
        let workspace_dir = current_workspace_dir(&data_dir);
        maybe_start_admin(host_admin::AdminState {
            agents_dir,
            skills_dir: data_dir.join("skills"),
            workspace_root_label: current_workspace_root_label(&workspace_dir),
            workspace_dir,
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
                return crate::channel::tui::TuiChannel::new(cli)
                    .run_once(runtime)
                    .await;
            }
            if let Some(message) = cli.once.clone() {
                if cli.pure_prompt {
                    process_prompt_message(Rc::clone(&runtime), &cli, message).await?;
                    wait_for_cli_background_tasks(Rc::clone(&runtime), &cli).await;
                    return Ok(());
                }
                process_cli_message(Rc::clone(&runtime), &cli, message).await?;
                wait_for_cli_background_tasks(Rc::clone(&runtime), &cli).await;
                return Ok(());
            }
            if cli.enabled {
                return crate::channel::cli::CliChannel::new(cli).run(runtime).await;
            }
            match gateway {
                Some(gateway) => {
                    crate::channel::feishu::FeishuChannel::new(gateway)
                        .run(runtime)
                        .await
                }
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
    tui_mode: bool,
    home_default: bool,
) -> anyhow::Result<InstanceProfile> {
    if tui_mode {
        if let Some(name) = cli_profile.or_else(|| std::env::var("REMI_PROFILE").ok()) {
            return InstanceProfile::named_in_data_root(&name, &tui_home_data_dir());
        }
        return Ok(InstanceProfile {
            name: None,
            data_dir: tui_home_data_dir(),
        });
    }
    if let Some(data_dir) = explicit_data_dir {
        return Ok(InstanceProfile {
            name: None,
            data_dir,
        });
    }
    if let Some(name) = cli_profile.or_else(|| std::env::var("REMI_PROFILE").ok()) {
        if home_default {
            return InstanceProfile::named_in_data_root(&name, &tui_home_data_dir());
        }
        return InstanceProfile::named(&name);
    }
    if !home_default {
        if let Some(data_dir) = std::env::var_os("REMI_DATA_DIR").map(PathBuf::from) {
            return Ok(InstanceProfile {
                name: None,
                data_dir,
            });
        }
    }
    if home_default {
        return Ok(InstanceProfile {
            name: None,
            data_dir: tui_home_data_dir(),
        });
    }
    Ok(InstanceProfile::default_instance())
}

fn absolute_env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
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

async fn wait_for_cli_background_tasks(runtime: Rc<Runtime>, cli: &CliConfig) {
    if !cli.wait_background_tasks {
        return;
    }
    while runtime.bot.is_thread_running(&cli.channel_id).await {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn run_tasks_command(data_dir: &Path, command: &TasksCommand) -> anyhow::Result<()> {
    let manager = bot_core::ToolTaskManager::load(data_dir)?;
    match command {
        TasksCommand::List { json } => {
            let tasks = manager.list(None).await;
            if *json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else if tasks.is_empty() {
                println!("no background tool tasks");
            } else {
                for task in tasks {
                    println!(
                        "{}\t{}\t{}\t{}\t{}ms",
                        task.task_id,
                        task.status,
                        task.thread_id,
                        task.tool_name,
                        task.elapsed_ms.unwrap_or(0)
                    );
                }
            }
        }
        TasksCommand::Get { task_id, json } => {
            let task = manager.get(task_id).await;
            if *json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else if let Some(task) = task {
                println!("task_id: {}", task.task_id);
                println!("status: {}", task.status);
                println!("thread_id: {}", task.thread_id);
                println!("tool: {}", task.tool_name);
                println!("elapsed_ms: {}", task.elapsed_ms.unwrap_or(0));
                if let Some(message) = task.message {
                    println!("message: {message}");
                }
                if !task.recent_output.is_empty() {
                    println!("recent_output:\n{}", task.recent_output.join("\n"));
                }
                if let Some(result) = task.result_preview {
                    println!("result:\n{result}");
                }
            } else {
                anyhow::bail!("task not found: {task_id}");
            }
        }
        TasksCommand::Cancel { task_id, json } => {
            let task = manager.cancel(task_id).await;
            if *json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else if let Some(task) = task {
                println!("{} {}", task.task_id, task.status);
            } else {
                anyhow::bail!("task not found: {task_id}");
            }
        }
    }
    Ok(())
}

fn current_workspace_dir(data_dir: &Path) -> PathBuf {
    std::env::var_os("REMI_SANDBOX_HOST_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| data_dir.to_path_buf())
}

fn current_workspace_root_label(workspace_dir: &Path) -> String {
    let kind = std::env::var("REMI_SANDBOX_KIND")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase());
    if matches!(kind.as_deref(), Some("docker")) {
        std::env::var("REMI_SANDBOX_CONTAINER_DIR")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "/workspace".to_string())
    } else {
        workspace_dir.display().to_string()
    }
}

fn init_observability(
    tui_enabled: bool,
    stdio_json: bool,
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

    if tui_enabled || stdio_json {
        let log_dir = data_dir.join("logs");
        std::fs::create_dir_all(&log_dir)?;
        let file_name = if stdio_json { "acp.log" } else { "tui.log" };
        let file_appender = tracing_appender::rolling::never(log_dir, file_name);
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

#[cfg(test)]
mod cli_tests {
    use super::{
        build_cargo_install_args, extract_first_url, extract_lark_cli_config_from_json,
        feedback_repo, feishu_doctor_message, feishu_session_channel_id, feishu_topic_channel_id,
        first_available_port, format_context_compaction_line, format_feishu_tool_line,
        github_new_issue_url, is_goal_set_command, normalize_release_tag, parse_cli_args,
        parse_command, parse_global_args, parse_goal_max_rounds, parse_release_version,
        parse_workflow_start_options, percent_encode_query, prefix_short_config_entry,
        redact_known_secrets, run_streaming_command, run_streaming_command_with_stdin,
        should_ignore_unaddressed_topic_start, try_parse_cli_args, update_available,
        AcpAdapterCommand, AcpCommand, AppCommand, CliConfig, CodexCommand, FeedbackCommand,
        FeishuCommand, FeishuDoctorStatus, FeishuReplyKind, GitHubIssueCreateRequest, HooksCommand,
        ProfileCommand, UpdateCommand,
    };
    use crate::direct_workflow_options;
    use crate::profile_command::{
        ProfileAgentCommand, ProfileWorkflowCommand, PROFILE_RUNTIME_ENV_KEYS,
    };
    use bot_core::{GoalMaxRounds, PrettyToolCall};
    use clap::error::ErrorKind;
    use im_feishu::FeishuMessage;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    fn global_tool_output_overflow_bytes_arg_is_parsed() {
        let cli =
            try_parse_cli_args(&args(&["--tool-output-overflow-bytes", "32768", "cli"])).unwrap();
        assert_eq!(cli.tool_output_overflow_bytes, Some(32_768));

        let cli = try_parse_cli_args(&args(&["--overflow-bytes=4096", "tui"])).unwrap();
        assert_eq!(cli.tool_output_overflow_bytes, Some(4_096));
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
    fn tui_resume_subcommand_accepts_user_options_after_session_id() {
        let command = parse_command(&args(&[
            "tui", "resume", "abc123", "--user", "u1", "--name", "Alice",
        ]))
        .unwrap();
        let AppCommand::Run(config) = command else {
            panic!("expected run command");
        };
        assert!(config.enabled);
        assert!(config.tui);
        assert!(config.resume);
        assert_eq!(config.resume_session_id.as_deref(), Some("abc123"));
        assert_eq!(config.user_id, "u1");
        assert_eq!(config.username, "Alice");
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
    fn tui_profile_uses_home_config_root_even_with_explicit_data_dir() {
        let profile = super::resolve_instance_profile(
            None,
            Some(std::path::PathBuf::from("/tmp/remi-data")),
            true,
            false,
        )
        .unwrap();
        assert_eq!(profile.name, None);
        assert_eq!(
            profile.data_dir,
            crate::instance_profile::tui_home_data_dir()
        );

        let named = super::resolve_instance_profile(
            Some("dev".to_string()),
            Some(std::path::PathBuf::from("/tmp/remi-data")),
            true,
            false,
        )
        .unwrap();
        assert_eq!(named.name.as_deref(), Some("dev"));
        assert_eq!(
            named.data_dir,
            crate::instance_profile::tui_home_data_dir()
                .join("profiles")
                .join("dev")
        );
    }

    #[test]
    fn acp_agent_defaults_to_home_config_root() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("REMI_PROFILE");
            std::env::remove_var("REMI_DATA_DIR");
        }

        let profile = super::resolve_instance_profile(None, None, false, true).unwrap();
        assert_eq!(profile.name, None);
        assert_eq!(
            profile.data_dir,
            crate::instance_profile::tui_home_data_dir()
        );

        let named =
            super::resolve_instance_profile(Some("dev".to_string()), None, false, true).unwrap();
        assert_eq!(named.name.as_deref(), Some("dev"));
        assert_eq!(
            named.data_dir,
            crate::instance_profile::tui_home_data_dir()
                .join("profiles")
                .join("dev")
        );

        let explicit = super::resolve_instance_profile(
            None,
            Some(std::path::PathBuf::from("/tmp/remi-data")),
            false,
            true,
        )
        .unwrap();
        assert_eq!(explicit.name, None);
        assert_eq!(
            explicit.data_dir,
            std::path::PathBuf::from("/tmp/remi-data")
        );
    }

    #[test]
    fn acp_agent_accepts_only_absolute_env_data_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_data_dir = std::env::var_os("REMI_DATA_DIR");
        let absolute = std::env::temp_dir().join(format!("remi-acp-{}", uuid::Uuid::new_v4()));

        unsafe {
            std::env::set_var("REMI_DATA_DIR", &absolute);
        }
        assert_eq!(super::absolute_env_path("REMI_DATA_DIR"), Some(absolute));

        unsafe {
            std::env::set_var("REMI_DATA_DIR", "relative-remi-data");
        }
        assert_eq!(super::absolute_env_path("REMI_DATA_DIR"), None);
        let profile = super::resolve_instance_profile(None, None, false, true).unwrap();
        assert_eq!(
            profile.data_dir,
            crate::instance_profile::tui_home_data_dir()
        );

        unsafe {
            match old_data_dir {
                Some(value) => std::env::set_var("REMI_DATA_DIR", value),
                None => std::env::remove_var("REMI_DATA_DIR"),
            }
        }
    }

    #[test]
    fn tui_legacy_env_applies_runtime_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        let data_root = std::env::temp_dir().join(format!(
            "remi-tui-runtime-defaults-{}",
            uuid::Uuid::new_v4()
        ));
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test-key");
            for key in [
                "REMI_DATA_DIR",
                "REMI_SANDBOX_KIND",
                "REMI_SANDBOX_HOST_DIR",
                "REMI_SHELL_MODE",
                "REMI_ACP_MODE",
                "REMI_ACP_CLIENT",
            ] {
                std::env::remove_var(key);
            }
        }

        let mut data_dir = data_root.clone();
        super::apply_runtime_env_defaults(&mut data_dir, true);

        assert_eq!(data_dir, data_root);
        assert_eq!(
            std::env::var("REMI_DATA_DIR").unwrap(),
            data_root.display().to_string()
        );
        assert_eq!(std::env::var("REMI_SANDBOX_KIND").unwrap(), "no_sandbox");
        assert_eq!(std::env::var("REMI_ACP_MODE").unwrap(), "local");
        assert_eq!(std::env::var("REMI_ACP_CLIENT").unwrap(), "codex");

        unsafe {
            for key in [
                "OPENAI_API_KEY",
                "REMI_DATA_DIR",
                "REMI_SANDBOX_KIND",
                "REMI_SANDBOX_HOST_DIR",
                "REMI_SHELL_MODE",
                "REMI_ACP_MODE",
                "REMI_ACP_CLIENT",
            ] {
                std::env::remove_var(key);
            }
        }
        let _ = std::fs::remove_dir_all(data_root);
    }

    #[test]
    fn admin_command_serves_management_api_only() {
        let config = CliConfig::from_args(&args(&["admin"])).unwrap();
        assert!(config.admin_only);
        assert!(!config.enabled);
    }

    #[test]
    fn clap_help_is_handled_before_runtime_command() {
        assert_eq!(
            try_parse_cli_args(&args(&["--help"])).unwrap_err().kind(),
            ErrorKind::DisplayHelp
        );
        assert_eq!(
            try_parse_cli_args(&args(&["help"])).unwrap_err().kind(),
            ErrorKind::DisplayHelp
        );
        assert_eq!(
            try_parse_cli_args(&args(&["profile", "agent", "--help"]))
                .unwrap_err()
                .kind(),
            ErrorKind::DisplayHelp
        );
        assert_eq!(
            try_parse_cli_args(&args(&["update", "self", "--help"]))
                .unwrap_err()
                .kind(),
            ErrorKind::DisplayHelp
        );
    }

    #[test]
    fn setup_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&["setup"])).unwrap(),
            AppCommand::Setup(entries) if entries.is_empty()
        ));
    }

    #[test]
    fn hooks_management_commands_are_recognized() {
        assert!(matches!(
            parse_command(&args(&["hooks"])).unwrap(),
            AppCommand::Hooks(HooksCommand::List { json: false })
        ));
        assert!(matches!(
            parse_command(&args(&["hooks", "list", "--json"])).unwrap(),
            AppCommand::Hooks(HooksCommand::List { json: true })
        ));
        assert!(matches!(
            parse_command(&args(&["hooks", "trust", "abc123"])).unwrap(),
            AppCommand::Hooks(HooksCommand::Trust { hash }) if hash == "abc123"
        ));
        assert!(matches!(
            parse_command(&args(&["hooks", "enable", "abc123"])).unwrap(),
            AppCommand::Hooks(HooksCommand::Enable { hash }) if hash == "abc123"
        ));
        assert!(matches!(
            parse_command(&args(&["hooks", "disable", "abc123"])).unwrap(),
            AppCommand::Hooks(HooksCommand::Disable { hash }) if hash == "abc123"
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
            parse_command(&args(&["workflow", "list"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Workflow(ProfileWorkflowCommand::List { profile }))
                if profile == "default"
        ));
        assert!(matches!(
            parse_command(&args(&["workflow", "show", "--profile", "dev", "verify"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Workflow(ProfileWorkflowCommand::Show { profile, workflow_id }))
                if profile == "dev" && workflow_id == "verify"
        ));
        assert!(matches!(
            parse_command(&args(&["workflow", "add", "/tmp/verify.json"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Workflow(ProfileWorkflowCommand::Upsert { profile, path }))
                if profile == "default" && path == "/tmp/verify.json"
        ));
        assert!(matches!(
            parse_command(&args(&["workflow", "rm", "--profile", "dev", "verify"])).unwrap(),
            AppCommand::Profile(ProfileCommand::Workflow(ProfileWorkflowCommand::Delete { profile, workflow_id }))
                if profile == "dev" && workflow_id == "verify"
        ));
        assert!(parse_command(&args(&["workflow", "rm", "goal"])).is_err());
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
    fn direct_workflow_options_require_exact_command_boundary() {
        assert_eq!(
            direct_workflow_options("review-loop", "review-loop"),
            Some("")
        );
        assert_eq!(
            direct_workflow_options("review-loop --max-rounds 1", "review-loop"),
            Some("--max-rounds 1")
        );
        assert_eq!(
            direct_workflow_options("Review Loop --context {}", "Review Loop"),
            Some("--context {}")
        );
        assert_eq!(
            direct_workflow_options("review-looping", "review-loop"),
            None
        );
    }

    #[test]
    fn workflow_start_options_accept_plain_goal_text() {
        let (context, max_rounds) = parse_workflow_start_options("verify the task").unwrap();
        assert_eq!(context, serde_json::json!({ "goal": "verify the task" }));
        assert_eq!(max_rounds, bot_core::WorkflowMaxRounds::Limited(20));

        let (context, max_rounds) =
            parse_workflow_start_options("--max-rounds 5 verify the task").unwrap();
        assert_eq!(context, serde_json::json!({ "goal": "verify the task" }));
        assert_eq!(max_rounds, bot_core::WorkflowMaxRounds::Limited(5));

        let (context, max_rounds) =
            parse_workflow_start_options("--context {\"goal\":\"verify the task\"}").unwrap();
        assert_eq!(context, serde_json::json!({ "goal": "verify the task" }));
        assert_eq!(max_rounds, bot_core::WorkflowMaxRounds::Limited(20));
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
    fn codex_setup_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&[
                "codex",
                "setup",
                "--bin",
                "/usr/local/bin/codex",
                "--agent",
                "default",
                "--arg=--config",
                "--arg=model=\"gpt-5-codex\""
            ]))
            .unwrap(),
            AppCommand::Codex(CodexCommand::Setup { bin, agent, args })
                if bin.as_deref() == Some("/usr/local/bin/codex")
                    && agent.as_deref() == Some("default")
                    && args == vec!["--config".to_string(), "model=\"gpt-5-codex\"".to_string()]
        ));
    }

    #[test]
    fn acp_setup_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&[
                "acp",
                "setup",
                "--client",
                "my-acp",
                "--mode",
                "remote",
                "--tool-name",
                "acp__my_acp",
                "--agent",
                "default",
                "--base-url",
                "http://127.0.0.1:8788",
                "--model",
                "gpt-5",
                "--api-key",
                "secret",
                "--arg=--verbose"
            ]))
            .unwrap(),
            AppCommand::Acp(AcpCommand::Setup {
                client,
                mode,
                tool_name,
                agent,
                base_url,
                model,
                api_key,
                bin,
                args
            })
                if client == "my-acp"
                    && mode.as_deref() == Some("remote")
                    && tool_name.as_deref() == Some("acp__my_acp")
                    && agent.as_deref() == Some("default")
                    && base_url.as_deref() == Some("http://127.0.0.1:8788")
                    && model.as_deref() == Some("gpt-5")
                    && api_key.as_deref() == Some("secret")
                    && bin.is_none()
                    && args == vec!["--verbose".to_string()]
        ));
    }

    #[test]
    fn acp_setup_remi_external_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&[
                "acp",
                "setup",
                "--client",
                "remi",
                "--bin",
                "/usr/local/bin/remi-cat",
                "--tool-name",
                "acp__remi"
            ]))
            .unwrap(),
            AppCommand::Acp(AcpCommand::Setup {
                client,
                bin,
                tool_name,
                args,
                ..
            })
                if client == "remi"
                    && bin.as_deref() == Some("/usr/local/bin/remi-cat")
                    && tool_name.as_deref() == Some("acp__remi")
                    && args.is_empty()
        ));
    }

    #[test]
    fn acp_adapter_codex_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&[
                "acp-adapter",
                "codex",
                "--bin",
                "/usr/local/bin/codex",
                "--arg=--config",
                "--arg=model=\"gpt-5-codex\""
            ]))
            .unwrap(),
            AppCommand::AcpAdapter(AcpAdapterCommand::Codex { bin, args })
                if bin.as_deref() == Some("/usr/local/bin/codex")
                    && args == vec!["--config".to_string(), "model=\"gpt-5-codex\"".to_string()]
        ));
    }

    #[test]
    fn acp_agent_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&["acp", "agent"])).unwrap(),
            AppCommand::Acp(AcpCommand::Agent)
        ));
    }

    #[test]
    fn codex_doctor_command_is_recognized() {
        assert!(matches!(
            parse_command(&args(&["codex", "doctor"])).unwrap(),
            AppCommand::Codex(CodexCommand::Doctor)
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
            "default\tRemi\tdefault\tsearch, skill__get, todo__add, todo__list, todo__complete, todo__update, todo__remove, memory__upsert_named, memory__get_detail, tool_tasks, bash, fs_read, fs_write, apply_patch, fs_mkdir, fs_remove, fs_ls, fetch, codex, manage_yourself",
            true,
            511,
        );

        let line = format_feishu_tool_line(&pretty);

        assert!(line.chars().count() <= 141);
        assert!(line.contains("default"));
        assert!(line.contains("Remi"));
        assert!(!line.contains("memory__upsert_named"));
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
    fn cli_message_can_wait_for_background_tasks() {
        let config =
            CliConfig::from_args(&args(&["--wait-background-tasks", "-m", "hello"])).unwrap();
        assert!(config.wait_background_tasks);
        assert_eq!(config.once.as_deref(), Some("hello"));
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
    fn prompt_subcommand_can_wait_for_background_tasks() {
        let parsed = parse_cli_args(&args(&[
            "prompt",
            "--wait-background-tasks",
            "--session",
            "s1",
            "hello",
        ]))
        .unwrap();
        let AppCommand::Run(config) = parsed.command else {
            panic!("expected run command");
        };
        assert!(config.wait_background_tasks);
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
