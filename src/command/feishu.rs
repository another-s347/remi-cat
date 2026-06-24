use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex;

use crate::config::load_dotenv_pairs;
use crate::secret_store::SecretStore;

#[derive(Debug, Clone, PartialEq, Eq)]
enum FeishuCredentialChoice {
    ReuseExisting,
    OverwriteWithLarkCli,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LarkCliConfigSnapshot {
    pub(crate) path: Option<PathBuf>,
    pub(crate) app_id: Option<String>,
    pub(crate) app_secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandRunSummary {
    pub(crate) lines: Vec<String>,
    pub(crate) first_url: Option<String>,
    pub(crate) success: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthStatusSummary {
    pub(crate) success: bool,
    pub(crate) output: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeishuDoctorStatus {
    pub(crate) lark_cli_installed: bool,
    pub(crate) lark_cli_config: Option<LarkCliConfigSnapshot>,
    pub(crate) auth_status: Option<AuthStatusSummary>,
    pub(crate) remi_app_id_present: bool,
    pub(crate) remi_app_secret_present: bool,
}

pub(crate) async fn run_feishu_init(secret_store: Arc<Mutex<SecretStore>>) -> anyhow::Result<()> {
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

pub(crate) async fn run_feishu_doctor() -> anyhow::Result<()> {
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

pub(crate) fn feishu_doctor_message(status: &FeishuDoctorStatus) -> Option<&'static str> {
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

pub(crate) async fn run_streaming_command(
    bin: &str,
    args: &[&str],
    url_hint: &str,
) -> anyhow::Result<CommandRunSummary> {
    run_streaming_command_with_stdin(bin, args, url_hint, None).await
}

pub(crate) async fn run_streaming_command_with_stdin(
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

pub(crate) fn extract_first_url(line: &str) -> Option<String> {
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

pub(crate) fn extract_lark_cli_config_from_json(json: &serde_json::Value) -> LarkCliConfigSnapshot {
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
