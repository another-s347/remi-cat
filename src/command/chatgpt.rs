use super::chatgpt_login::{open_browser, CallbackHost, CallbackServer};
use crate::instance_profile::InstanceProfile;
use anyhow::{ensure, Context, Result};
use bot_core::{
    chatgpt::{ChatGptAuthManager, ChatGptResponsesClient},
    ContextCompactionMode, ModelProfileConfig, ModelProfileRegistry,
};
use clap::Subcommand;
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub(crate) enum ChatGptCommand {
    /// Continue with ChatGPT to authorize subscription usage for this profile.
    Login {
        #[arg(long)]
        no_browser: bool,
        /// Replace the saved registration after signing out.
        #[arg(long)]
        new_account: bool,
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long, value_enum, default_value = "auto")]
        callback_host: CallbackHost,
    },
    /// Log in, select a subscription model, verify it, and configure the default.
    Setup {
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value = "chatgpt-main")]
        id: String,
        /// Local context budget; keep within the chosen model's supported capacity.
        #[arg(long, default_value_t = 32768)]
        context_tokens: u32,
        /// Local output reservation, not a server generation limit.
        #[arg(long, default_value_t = 4096)]
        max_output_tokens: u32,
        #[arg(long)]
        no_browser: bool,
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long, value_enum, default_value = "auto")]
        callback_host: CallbackHost,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Logout,
    Models {
        #[arg(long)]
        json: bool,
    },
    Model {
        #[command(subcommand)]
        command: ChatGptModelCommand,
    },
    Check {
        #[arg(long)]
        model_profile: String,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub(crate) enum ChatGptModelCommand {
    Add {
        slug: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        context_tokens: u32,
        #[arg(long)]
        max_output_tokens: u32,
        #[arg(long)]
        set_default: bool,
    },
}

pub(crate) async fn run(profile: &InstanceProfile, command: &ChatGptCommand) -> Result<()> {
    let auth = ChatGptAuthManager::new(&profile.data_dir);
    match command {
        ChatGptCommand::Login {
            no_browser,
            new_account,
            port,
            callback_host,
        } => {
            print_location(profile);
            login(auth, *no_browser, *new_account, *port, *callback_host).await?;
        }
        ChatGptCommand::Setup {
            model,
            id,
            context_tokens,
            max_output_tokens,
            no_browser,
            port,
            callback_host,
        } => {
            print_location(profile);
            ensure!(valid_id(id), "invalid model profile ID");
            // Validate the existing configuration before starting browser authorization.
            let mut runtime = crate::runtime_config::load_runtime_config_at(
                &profile.runtime_config,
                &profile.data_dir,
            )?
            .unwrap_or_else(|| {
                crate::runtime_config::RuntimeConfig::default_for(&profile.data_dir)
            });
            let status = auth.status()?;
            if !status.signed_in || !status.plan_authorized || status.needs_login {
                login(auth.clone(), *no_browser, false, *port, *callback_host).await?;
            } else {
                println!("Using the ChatGPT account already authorized for this profile.");
            }
            let catalog = auth.models().await?;
            let visible = visible_models(&catalog)?;
            let slug = match model {
                Some(slug) => slug.clone(),
                None => choose_model(&visible)?,
            };
            let entry = visible
                .iter()
                .find(|entry| entry["slug"].as_str() == Some(&slug))
                .context("selected model is not listed for this account")?;
            let mut config =
                subscription_model(entry, id, &slug, *context_tokens, *max_output_tokens);
            let path = model_destination(profile, id);
            if path.exists() {
                let existing = ModelProfileRegistry::load(&profile.models_dir)?
                    .get(id)
                    .cloned()
                    .context("existing model cannot be loaded")?;
                ensure!(
                    existing.provider.as_deref() == Some("chatgpt") && existing.model == slug,
                    "model profile ID is already used by another model; choose another --id"
                );
                config = existing;
                println!("Keeping existing model profile: {}", path.display());
            }
            config.validate()?;
            println!(
                "Verifying {} with a real subscription request...",
                config.name
            );
            let reply = ChatGptResponsesClient::new(auth)
                .complete_text(
                    &slug,
                    "Reply briefly.",
                    "Say hello.",
                    config.merged_extra_options(None, None)?,
                )
                .await?;
            println!("Model replied: {reply}");
            // Commit model selection only after nonempty text and successful termination.
            if !path.exists() {
                std::fs::create_dir_all(&profile.models_dir)?;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                file.write_all(serde_yaml::to_string(&config)?.as_bytes())?;
                file.sync_all()?;
            }
            bot_core::install_embedded_agent_profiles(&profile.agents_dir)?;
            runtime.model_profile = id.clone();
            crate::runtime_config::write_runtime_config_at(&profile.runtime_config, &runtime)?;
            println!(
                "Default model: {id} ({slug}). Configuration: {}",
                profile.runtime_config.display()
            );
            println!("Ready. Start `remi-cat tui` with the same --profile selection.");
        }
        ChatGptCommand::Status { json } => {
            let s = auth.status()?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                println!(
                    "account: {}\nauthorized: {}\nneeds_login: {}\nexpires_at: {}\nusage: {}",
                    s.email
                        .as_deref()
                        .or(s.subject.as_deref())
                        .unwrap_or("signed out"),
                    s.signed_in && s.plan_authorized && !s.needs_login,
                    s.needs_login,
                    s.expires_at
                        .and_then(|v| chrono::DateTime::from_timestamp(v as i64, 0))
                        .map(|v| v.to_rfc3339())
                        .unwrap_or_else(|| "unknown".into()),
                    s.usage_url
                );
            }
        }
        ChatGptCommand::Logout => {
            let revoked = auth.logout().await?;
            println!("Signed out of ChatGPT for this profile.");
            if !revoked {
                println!("Remote revocation was not confirmed; disconnect remi-cat in ChatGPT Settings if needed.");
            }
        }
        ChatGptCommand::Models { json } => {
            let catalog = auth.models().await?;
            let list: Vec<_> = catalog["models"]
                .as_array()
                .context("model catalog missing")?
                .iter()
                .filter(|m| m["visibility"] == "list")
                .collect();
            if *json {
                println!("{}", serde_json::to_string_pretty(&list)?);
            } else {
                for model in list {
                    println!(
                        "{}\t{}",
                        model["slug"].as_str().unwrap_or(""),
                        model["display_name"].as_str().unwrap_or("")
                    );
                }
            }
        }
        ChatGptCommand::Model {
            command:
                ChatGptModelCommand::Add {
                    slug,
                    id,
                    context_tokens,
                    max_output_tokens,
                    set_default,
                },
        } => {
            ensure!(
                valid_id(id),
                "model profile ID must be 1-64 ASCII letters, digits, hyphens or underscores"
            );
            let catalog = auth.models().await?;
            let model = catalog["models"]
                .as_array()
                .context("model catalog missing")?
                .iter()
                .find(|m| m["slug"].as_str() == Some(slug) && m["visibility"] == "list")
                .context("model is not listed for this ChatGPT account")?;
            let config = subscription_model(model, id, slug, *context_tokens, *max_output_tokens);
            config.validate()?;
            let mut runtime = if *set_default {
                Some(
                    crate::runtime_config::load_runtime_config_at(
                        &profile.runtime_config,
                        &profile.data_dir,
                    )?
                    .context("initialize this profile before selecting its default model")?,
                )
            } else {
                None
            };
            let path = model_destination(profile, id);
            ensure!(
                !path.exists(),
                "model profile already exists: {}",
                path.display()
            );
            std::fs::create_dir_all(&profile.models_dir)?;
            let raw = serde_yaml::to_string(&config)?;
            // Never overwrite an existing profile, including a concurrent creation.
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            use std::io::Write;
            file.write_all(raw.as_bytes())?;
            file.sync_all()?;
            println!("Created {}", path.display());
            if let Some(runtime) = runtime.as_mut() {
                runtime.model_profile = id.clone();
                crate::runtime_config::write_runtime_config_at(&profile.runtime_config, runtime)?;
                println!("Default model: {id}");
            }
            println!("Run `remi-cat chatgpt check --model-profile {id}` to verify inference. Use the same --profile selection.");
        }
        ChatGptCommand::Check { model_profile } => {
            let registry = ModelProfileRegistry::load(&profile.models_dir)?;
            let model = registry
                .get(model_profile)
                .context("unknown model profile")?;
            ensure!(
                model.provider.as_deref() == Some("chatgpt"),
                "selected model does not use ChatGPT subscription"
            );
            model.validate()?;
            let text = ChatGptResponsesClient::new(auth)
                .complete_text(
                    &model.model,
                    "Reply briefly.",
                    "Say hello.",
                    model.merged_extra_options(None, None)?,
                )
                .await?;
            println!("{text}\nInference completed successfully using ChatGPT subscription.");
        }
    }
    Ok(())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn model_destination(profile: &InstanceProfile, id: &str) -> PathBuf {
    profile.models_dir.join(format!("{id}.yaml"))
}
fn print_location(profile: &InstanceProfile) {
    println!(
        "Profile: {}\nCredentials: {}\nConfiguration: {}",
        profile.label(),
        profile.data_dir.join("auth/chatgpt").display(),
        profile.runtime_config.display()
    );
}
fn visible_models(catalog: &serde_json::Value) -> Result<Vec<&serde_json::Value>> {
    let models: Vec<_> = catalog["models"]
        .as_array()
        .context("model catalog missing")?
        .iter()
        .filter(|model| model["visibility"] == "list" && model["slug"].as_str().is_some())
        .collect();
    ensure!(
        !models.is_empty(),
        "No models are available for this ChatGPT account or workspace"
    );
    Ok(models)
}
fn choose_model(models: &[&serde_json::Value]) -> Result<String> {
    ensure!(
        std::io::stdin().is_terminal(),
        "Model selection requires a terminal; supply --model <slug> for non-interactive setup"
    );
    println!("Available subscription models:");
    for (index, model) in models.iter().enumerate() {
        println!(
            "  {}. {} ({})",
            index + 1,
            model["display_name"].as_str().unwrap_or(""),
            model["slug"].as_str().unwrap_or("")
        );
    }
    loop {
        print!("Select model [1]: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        ensure!(
            std::io::stdin().read_line(&mut answer)? != 0,
            "Model selection cancelled (input closed)"
        );
        let index = if answer.trim().is_empty() {
            Some(1)
        } else {
            answer.trim().parse::<usize>().ok()
        };
        if let Some(model) = index
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| models.get(i))
        {
            return Ok(model["slug"].as_str().context("model slug missing")?.into());
        }
        println!("Enter a number from 1 to {}.", models.len());
    }
}
fn subscription_model(
    entry: &serde_json::Value,
    id: &str,
    slug: &str,
    context_tokens: u32,
    max_output_tokens: u32,
) -> ModelProfileConfig {
    ModelProfileConfig {
        id: id.into(), name: entry["display_name"].as_str().unwrap_or(slug).into(), model: slug.into(),
        provider: Some("chatgpt".into()), base_url: None, thinking: None, reasoning_effort: None,
        context_tokens, max_output_tokens, supports_images: entry["supports_images"].as_bool().unwrap_or(false),
        legacy_short_term_tokens: None, overflow_bytes: 32_768, context_compaction: ContextCompactionMode::Hard,
        description: Some("ChatGPT subscription via OAuth. max_output_tokens reserves local context; it is not a server output cap.".into()),
        extra_options: Default::default(),
    }
}

async fn login(
    auth: ChatGptAuthManager,
    no_browser: bool,
    new_account: bool,
    port: u16,
    callback_host: CallbackHost,
) -> Result<()> {
    let mut server = CallbackServer::bind(callback_host, port).await?;
    let port = server.port()?;
    let attempt = if new_account {
        auth.begin_login_new_account(port).await?
    } else {
        auth.begin_login(port).await?
    };
    let windows = server.is_windows();
    println!(
        "Callback: {} loopback, port {port}.",
        if windows { "Windows" } else { "local" }
    );
    if no_browser || !open_browser(&attempt.authorization_url, windows).await {
        println!(
            "Open this link in your browser:\n{}",
            attempt.public_authorization_url()?
        );
        if !windows {
            println!("For SSH, forward this port from your browser's machine: ssh -L {port}:127.0.0.1:{port} <host>");
        }
    } else {
        println!("Browser opened. Continue with ChatGPT, then return here.");
    }
    println!("Waiting for authorization (30 minutes). Keep this terminal open; Ctrl+C cancels. Do not share callback URLs or authorization codes.");
    let received = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(1800), server.receive(attempt.callback_state())) => result.context("ChatGPT login timed out. Run `remi-cat chatgpt login` again and use the new browser attempt")?,
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("ChatGPT login cancelled; existing authorization is unchanged")),
    };
    server.stop().await;
    auth.finish_login(attempt, received?).await?;
    println!("ChatGPT subscription authorized. Default model unchanged.");
    println!("Next: remi-cat chatgpt setup (select a model and verify inference).");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_cannot_escape_model_directory() {
        for id in ["", "../x", "/tmp/x", "a.b", "a/b", "a\\b"] {
            assert!(!valid_id(id));
        }
        assert!(valid_id("chatgpt-gpt6_1"));
    }
    #[test]
    fn cli_parses_subscription_commands_and_global_profile() {
        for args in [
            vec!["chatgpt", "setup", "--model", "gpt-6-luna"],
            vec!["chatgpt", "login", "--callback-host", "windows"],
            vec![
                "--profile",
                "@work",
                "chatgpt",
                "login",
                "--no-browser",
                "--port",
                "1455",
            ],
            vec!["chatgpt", "status", "--json"],
            vec!["chatgpt", "logout"],
            vec!["chatgpt", "models", "--json"],
            vec![
                "chatgpt",
                "model",
                "add",
                "test-model",
                "--id",
                "chatgpt-main",
                "--context-tokens",
                "32768",
                "--max-output-tokens",
                "4096",
                "--set-default",
            ],
            vec!["chatgpt", "check", "--model-profile", "chatgpt-main"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            let parsed = crate::cli::parse_cli_args(&args).unwrap();
            assert!(matches!(parsed.command, crate::cli::AppCommand::ChatGpt(_)));
        }
    }
}
