mod host_admin;
mod runtime_config;
mod session;

use anyhow::Context;
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
    install_embedded_agent_profiles, install_embedded_model_profiles, AgentProfile, AgentRegistry,
    CatBot, CatBotBuilder, CatEvent, Content, ContentPart, GoalMaxRounds, ImAttachment, ImDocument,
    ModelProfileRegistry, StreamOptions,
};
use futures::StreamExt;
use im_feishu::{FeishuEvent, FeishuEventHookConfig, FeishuGateway, FeishuMessage, StreamingCard};
use remi_agentloop::types::{SubSessionEvent, SubSessionEventPayload};
use runtime_config::{
    detect_setup_state, has_legacy_env_credentials, load_dotenv_pairs, upsert_dotenv_value,
    write_runtime_config, FeishuTransport, RuntimeConfig, RuntimeSandboxKind, SetupState,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppCommand {
    Run(CliConfig),
    Setup,
    Doctor,
    Feishu(FeishuCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FeishuCommand {
    Init,
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliConfig {
    enabled: bool,
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
        Some("setup") => Ok(AppCommand::Setup),
        Some("doctor") | Some("check") => Ok(AppCommand::Doctor),
        Some("feishu") => Ok(AppCommand::Feishu(parse_feishu_command(&args[1..])?)),
        _ => Ok(AppCommand::Run(CliConfig::from_args(args)?)),
    }
}

fn parse_feishu_command(args: &[String]) -> anyhow::Result<FeishuCommand> {
    match args.first().map(String::as_str) {
        Some("init") => Ok(FeishuCommand::Init),
        Some("doctor") | Some("check") => Ok(FeishuCommand::Doctor),
        Some(other) => anyhow::bail!("unknown `remi-cat feishu` subcommand `{other}`"),
        None => anyhow::bail!("usage: remi-cat feishu <init|doctor>"),
    }
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
    let _ = dotenvy::dotenv();
    init_observability();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = parse_command(&args)?;
    let mut data_dir = initial_data_dir_for_command(&command);
    std::fs::create_dir_all(&data_dir)?;

    match command {
        AppCommand::Setup => {
            run_setup(&mut data_dir).await?;
            return Ok(());
        }
        AppCommand::Doctor => {
            run_doctor(&data_dir)?;
            return Ok(());
        }
        AppCommand::Feishu(FeishuCommand::Init) => {
            run_feishu_init().await?;
            return Ok(());
        }
        AppCommand::Feishu(FeishuCommand::Doctor) => {
            run_feishu_doctor().await?;
            return Ok(());
        }
        AppCommand::Run(ref cli) => {
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
            sessions,
            root_agent_id,
            setup_state: detect_setup_state(&data_dir),
        })
        .await?;
        tokio::signal::ctrl_c().await?;
        return Ok(());
    }

    let gateway = if cli.enabled || cli.once.is_some() {
        None
    } else {
        let app_id = std::env::var("FEISHU_APP_ID")
            .map_err(|_| anyhow::anyhow!("FEISHU_APP_ID must be set"))?;
        let app_secret = std::env::var("FEISHU_APP_SECRET")
            .map_err(|_| anyhow::anyhow!("FEISHU_APP_SECRET must be set"))?;
        Some(FeishuGateway::new(app_id, app_secret))
    };

    let bridge: Arc<dyn ImFileBridge> = Arc::new(LocalImFileBridge {
        gateway: gateway.clone(),
    });
    let bot = Rc::new(
        CatBotBuilder::from_env()?
            .im_bridge(Arc::clone(&bridge))
            .build()?,
    );
    let sessions = Arc::new(Mutex::new(SessionRuntime::load(data_dir.clone())?));
    if !cli.pure_prompt {
        maybe_start_admin(host_admin::AdminState {
            agents_dir,
            sessions: Arc::clone(&sessions),
            root_agent_id: root_agent_id.clone(),
            setup_state: detect_setup_state(&data_dir),
        })
        .await?;
    }

    let runtime = Rc::new(Runtime {
        bot,
        user_store: Arc::new(UserStore::load(data_dir.join("users.json"))?),
        sessions,
        im_bridge: Arc::clone(&bridge),
        root_agent_id,
        data_dir: data_dir.clone(),
    });

    let local_set = tokio::task::LocalSet::new();
    local_set
        .run_until(async move {
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
            run_feishu(runtime, gateway.expect("gateway should exist")).await
        })
        .await
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

fn init_observability() {
    let _sentry_guard = sentry::init((
        std::env::var("SENTRY_DSN").unwrap_or_default(),
        sentry::ClientOptions {
            release: sentry::release_name!(),
            ..Default::default()
        },
    ));
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer().with_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "remi_cat=info,bot_core=info,im_feishu=info".into()),
            ),
        )
        .with(sentry::integrations::tracing::layer())
        .init();
}

async fn run_setup(data_dir: &mut std::path::PathBuf) -> anyhow::Result<()> {
    println!("remi-cat setup");
    println!("This wizard will configure the local runtime and verify one real chat round.\n");

    let chosen_dir = prompt_with_default("Data dir", &data_dir.display().to_string())?;
    *data_dir = std::path::PathBuf::from(chosen_dir);
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
    config.im.transport = match feishu_transport.as_str() {
        "event_hook" | "event-hook" | "hook" => FeishuTransport::EventHook,
        _ => FeishuTransport::WebSocket,
    };
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

    let runtime_path = write_runtime_config(data_dir, &config)?;
    match run_setup_smoke(data_dir, &config, api_key.trim()).await {
        Ok(reply) => {
            let env_path = std::path::PathBuf::from(".env");
            upsert_dotenv_value(&env_path, "REMI_DATA_DIR", &config.data_dir)?;
            if !api_key.trim().is_empty() {
                upsert_dotenv_value(&env_path, "OPENAI_API_KEY", api_key.trim())?;
            }
            println!("\nSetup verification succeeded.");
            println!("Smoke reply: {}", reply.trim());
            println!("Saved runtime config to {}", runtime_path.display());
            println!("\nNext steps:");
            println!(
                "- Local chat: remi-cat cli --channel support --user alice --name Alice \"Hello\""
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

fn initial_data_dir_for_command(command: &AppCommand) -> PathBuf {
    match command {
        AppCommand::Setup => preferred_setup_data_dir(),
        _ => std::env::var("REMI_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(".remi-cat")),
    }
}

fn preferred_setup_data_dir() -> PathBuf {
    if let Ok(value) = std::env::var("REMI_DATA_DIR") {
        let candidate = PathBuf::from(value);
        if matches!(
            detect_setup_state(&candidate),
            SetupState::Initialized { .. }
        ) {
            return candidate;
        }
    }
    PathBuf::from(".remi-cat")
}

fn run_doctor(data_dir: &Path) -> anyhow::Result<()> {
    let setup_state = detect_setup_state(data_dir);
    println!("remi-cat doctor");
    println!("data_dir: {}", data_dir.display());
    println!("runtime_config: {}", setup_state.config_path().display());
    match &setup_state {
        SetupState::Initialized { config, .. } => {
            println!("setup: initialized");
            println!("root_agent_id: {}", config.root_agent_id);
            println!("model_profile: {}", config.model_profile);
            println!("feishu_transport: {}", config.im.transport.as_env_value());
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

async fn run_feishu_init() -> anyhow::Result<()> {
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
        println!("\nFeishu login verification succeeded.");
        println!("Existing FEISHU_APP_ID / FEISHU_APP_SECRET were kept as-is.");
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

    let env_path = PathBuf::from(".env");
    upsert_dotenv_value(&env_path, "FEISHU_APP_ID", app_id)?;
    upsert_dotenv_value(&env_path, "FEISHU_APP_SECRET", app_secret)?;
    unsafe {
        std::env::set_var("FEISHU_APP_ID", app_id);
        std::env::set_var("FEISHU_APP_SECRET", app_secret);
    }

    println!("\nFeishu setup completed successfully.");
    if let Some(path) = snapshot.path {
        println!("Imported app credentials from {}", path.display());
    }
    println!("Updated .env with FEISHU_APP_ID / FEISHU_APP_SECRET.");
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
    let mut stream = std::pin::pin!(runtime.bot.stream(&session_id, text));
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
    let mut card = gateway.begin_streaming_reply(&msg.message_id);
    collect_bot_reply(
        runtime,
        FEISHU_CHANNEL,
        msg.clone(),
        sender_username,
        Some(&mut card),
    )
    .await?;
    card.finish().await.ok();
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
    msg: FeishuMessage,
    sender_username: Option<String>,
    mut card: Option<&mut StreamingCard>,
) -> anyhow::Result<String> {
    let channel_id = feishu_session_channel_id(&msg);
    if msg.text.trim() == "/tools" {
        let reply = runtime
            .bot
            .tool_list()
            .into_iter()
            .map(|(name, desc)| format!("- `{name}`: {desc}"))
            .collect::<Vec<_>>()
            .join("\n");
        append_reply_chunk(&mut String::new(), &mut card, &reply).await;
        return Ok(reply);
    }
    if msg.text.trim().starts_with("/goal") {
        let session_id = runtime.sessions.lock().await.resolve_channel(
            platform,
            &channel_id,
            &runtime.root_agent_id,
        )?;
        let reply = handle_goal_command(&runtime.bot, &session_id, msg.text.trim()).await?;
        append_reply_chunk(&mut String::new(), &mut card, &reply).await;
        return Ok(reply);
    }
    if msg.text.trim() == "/compact" {
        let session_id = runtime.sessions.lock().await.resolve_channel(
            platform,
            &channel_id,
            &runtime.root_agent_id,
        )?;
        let n = runtime.bot.compact_memory(&session_id).await?;
        let reply = format!("compacted {n} short-term message(s).");
        append_reply_chunk(&mut String::new(), &mut card, &reply).await;
        return Ok(reply);
    }
    if msg.text.trim() == "/clear" {
        let session_id = runtime.sessions.lock().await.resolve_channel(
            platform,
            &channel_id,
            &runtime.root_agent_id,
        )?;
        runtime.bot.clear_memory(&session_id).await?;
        let reply = "已清空当前 session 的历史会话。Todo/trigger 等工具状态已保留。".to_string();
        append_reply_chunk(&mut String::new(), &mut card, &reply).await;
        return Ok(reply);
    }
    if msg.text.trim() == "/doctor" {
        let reply = command_doctor_report(&runtime);
        append_reply_chunk(&mut String::new(), &mut card, &reply).await;
        return Ok(reply);
    }

    let session_id = runtime.sessions.lock().await.resolve_channel(
        platform,
        &channel_id,
        &runtime.root_agent_id,
    )?;

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
    let opts = StreamOptions {
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
    let mut output = String::new();
    let buffer_for_goal = runtime.bot.goal_status(&session_id).await.is_some();
    let mut buffered_card = if buffer_for_goal { card.take() } else { None };
    let mut supervisor_prefix: Option<String> = None;
    let mut stream = std::pin::pin!(runtime.bot.stream_with_options(&session_id, content, opts));
    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else { break };
                match event {
                    CatEvent::Text(delta) => {
                        if buffer_for_goal {
                            output.push_str(&delta);
                        } else {
                            append_reply_chunk(&mut output, &mut card, &delta).await;
                        }
                    }
                    CatEvent::Thinking(content) => {
                        let chunk = format!("\n\n**Thinking**\n{}\n", fenced_block("text", &content));
                        if buffer_for_goal {
                            output.push_str(&chunk);
                        } else {
                            append_reply_chunk(&mut output, &mut card, &chunk).await;
                        }
                    }
                    CatEvent::ToolCall { name, args } => {
                        let json = serde_json::to_string_pretty(&args).unwrap_or_default();
                        let chunk = format!("\n\n**Tool `{name}`**\n{}\n", fenced_block("json", &json));
                        if buffer_for_goal {
                            output.push_str(&chunk);
                        } else {
                            append_reply_chunk(&mut output, &mut card, &chunk).await;
                        }
                    }
                    CatEvent::ToolCallResult { name, result } => {
                        let chunk = format!(
                            "\n\n**Tool result `{name}`**\n{}\n",
                            fenced_block("text", &result)
                        );
                        if buffer_for_goal {
                            output.push_str(&chunk);
                        } else {
                            append_reply_chunk(&mut output, &mut card, &chunk).await;
                        }
                    }
                    CatEvent::SubSession(event) => {
                        record_sub_session_event(&runtime, &session_id, platform, &msg, &event).await;
                        let chunk = format!(
                            "\n\n**Sub-session** `{}` / `{}`\n",
                            event.agent_name, event.sub_thread_id.0
                        );
                        if buffer_for_goal {
                            output.push_str(&chunk);
                        } else {
                            append_reply_chunk(&mut output, &mut card, &chunk).await;
                        }
                    }
                    CatEvent::Supervisor(report) => {
                        supervisor_prefix = Some(bot_core::goal::format_goal_prefix(&report));
                    }
                    CatEvent::Stats { prompt_tokens, completion_tokens, elapsed_ms } => {
                        let chunk = format!(
                            "\n\n---\n**调试信息**\n\n**Stats** `tokens: {prompt_tokens}->{completion_tokens}` `elapsed: {elapsed_ms}ms`"
                        );
                        if buffer_for_goal {
                            output.push_str(&chunk);
                        } else {
                            append_reply_chunk(&mut output, &mut card, &chunk).await;
                        }
                    }
                    CatEvent::Error(err) => {
                        let chunk = format!(
                            "\n\n---\n**调试信息**\n\n**Error**\n{}",
                            fenced_block("text", &err.to_string())
                        );
                        if buffer_for_goal {
                            output.push_str(&chunk);
                        } else {
                            append_reply_chunk(&mut output, &mut card, &chunk).await;
                        }
                        break;
                    }
                    CatEvent::Done => break,
                    _ => {}
                }
            }
            _ = &mut timeout => {
                let chunk = "\n\n---\n**调试信息**\n\n**Timeout** reply timed out";
                if buffer_for_goal {
                    output.push_str(chunk);
                } else {
                    append_reply_chunk(&mut output, &mut card, chunk).await;
                }
                break;
            }
        }
    }
    if output.trim().is_empty() {
        if buffer_for_goal {
            output.push_str("（无响应）");
        } else {
            append_reply_chunk(&mut output, &mut card, "（无响应）").await;
        }
    }
    if buffer_for_goal {
        if let Some(prefix) = supervisor_prefix {
            output = format!("{prefix}{output}");
        }
        append_reply_chunk(&mut String::new(), &mut buffered_card, &output).await;
    }
    Ok(output)
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
    let Some(mut goal_text) = rest.strip_prefix("set").map(str::trim) else {
        return Ok(
            "用法：/goal set [--max-rounds N|unlimited] <目标>，/goal status，/goal clear"
                .to_string(),
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
            return format!("{}:thread:{thread_id}", msg.chat_id);
        }
    }
    msg.chat_id.clone()
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

async fn append_reply_chunk(
    output: &mut String,
    card: &mut Option<&mut StreamingCard>,
    chunk: &str,
) {
    output.push_str(chunk);
    if let Some(card) = card.as_deref_mut() {
        card.push(chunk).await.ok();
    }
}

fn fenced_block(lang: &str, content: &str) -> String {
    let sanitized = content.replace("```", "'''");
    format!("```{lang}\n{}\n```", sanitized.trim())
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

#[cfg(test)]
mod cli_tests {
    use super::{
        extract_first_url, extract_lark_cli_config_from_json, feishu_doctor_message,
        feishu_session_channel_id, parse_command, parse_goal_max_rounds, run_streaming_command,
        run_streaming_command_with_stdin, should_ignore_unaddressed_topic_start, AppCommand,
        CliConfig, FeishuCommand, FeishuDoctorStatus,
    };
    use bot_core::GoalMaxRounds;
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
        assert_eq!(config.once, None);
        assert!(!config.pure_prompt);
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
            AppCommand::Setup
        ));
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
