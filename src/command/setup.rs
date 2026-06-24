use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Context;
use futures::StreamExt;
use tokio::sync::Mutex;

use crate::config::{
    detect_setup_state, upsert_dotenv_value, write_runtime_config, FeishuTransport, ImMode,
    RuntimeConfig, RuntimeSandboxKind, SetupState,
};
use crate::instance_profile::InstanceProfile;
use crate::profile_command::{
    available_container_name, configured_ports, first_available_port, print_port_adjustment,
};
use crate::secret_store::SecretStore;
use bot_core::{
    install_embedded_agent_profiles, install_embedded_model_profiles, AgentProfile, AgentRegistry,
    CatBotBuilder, CatEvent, Content, ModelProfileConfig, ModelProfileRegistry, StreamOptions,
};

pub(crate) async fn run_setup(
    profile: &InstanceProfile,
    data_dir: &mut PathBuf,
    secret_store: Arc<Mutex<SecretStore>>,
) -> anyhow::Result<()> {
    println!("remi-cat setup");
    println!("Profile: {}", profile.label());
    println!("This wizard will configure the local runtime and verify one real chat round.\n");

    if !profile.is_named() {
        let chosen_dir = prompt_with_default("Data dir", &data_dir.display().to_string())?;
        *data_dir = PathBuf::from(chosen_dir);
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
    let selected_model_profile = models
        .iter()
        .find(|profile| profile.id == model_profile)
        .ok_or_else(|| {
            anyhow::anyhow!("selected model profile `{model_profile}` no longer exists")
        })?;
    let api_key_env_key = api_key_env_key_for_profile(selected_model_profile);
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

    let current_api_key = api_key_env_keys_for_profile(selected_model_profile)
        .into_iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    let api_key = prompt_secret(current_api_key.as_deref())?;
    if !api_key.trim().is_empty() {
        unsafe {
            std::env::set_var(api_key_env_key, api_key.trim());
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
    match run_setup_smoke(data_dir, &config, api_key_env_key, api_key.trim()).await {
        Ok(reply) => {
            let env_path = PathBuf::from(".env");
            if !profile.is_named() {
                upsert_dotenv_value(&env_path, "REMI_DATA_DIR", &config.data_dir)?;
            }
            if !api_key.trim().is_empty() {
                secret_store
                    .lock()
                    .await
                    .set(api_key_env_key, api_key.trim())?;
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

async fn run_setup_smoke(
    data_dir: &Path,
    config: &RuntimeConfig,
    api_key_env_key: &str,
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
            std::env::set_var(api_key_env_key, api_key.trim());
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

fn api_key_env_key_for_profile(profile: &ModelProfileConfig) -> &'static str {
    api_key_env_keys_for_profile(profile)
        .into_iter()
        .next()
        .unwrap_or("OPENAI_API_KEY")
}

fn api_key_env_keys_for_profile(profile: &ModelProfileConfig) -> Vec<&'static str> {
    let provider = profile
        .provider
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let model = profile.model.to_ascii_lowercase();
    let base_url = profile
        .base_url
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();

    let mut keys =
        if provider == "mimo" || model.contains("mimo") || base_url.contains("xiaomimimo.com") {
            vec!["MIMO_API_KEY"]
        } else if provider == "kimi"
            || provider == "moonshot"
            || model.contains("kimi")
            || base_url.contains("moonshot.cn")
        {
            vec!["MOONSHOT_API_KEY", "KIMI_API_KEY"]
        } else if provider == "glm"
            || provider == "zhipu"
            || provider == "bigmodel"
            || model.contains("glm")
            || base_url.contains("bigmodel.cn")
        {
            vec!["GLM_API_KEY", "ZHIPU_API_KEY", "BIGMODEL_API_KEY"]
        } else {
            Vec::new()
        };
    keys.extend(["OPENAI_API_KEY", "REMI_API_KEY"]);
    keys
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
