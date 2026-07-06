use std::path::{Path, PathBuf};

use crate::cli::{AcpCommand, CodexCommand};
use crate::config::{
    detect_setup_state, AcpClient, AcpMode, FeishuTransport, RuntimeConfig, RuntimeSandboxKind,
    SetupState,
};
use crate::core::Runtime;
use crate::instance_profile::InstanceProfile;
use crate::profile_command::apply_runtime_config_entries;
use bot_core::{api_key_from_env, AgentRegistry, CatBot, ModelProfileRegistry};

pub(crate) fn run_doctor(profile: &InstanceProfile, data_dir: &Path) -> anyhow::Result<()> {
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
                "tool_output_overflow_bytes: {}",
                config
                    .tool_output
                    .overflow_bytes
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "model_profile_default".to_string())
            );
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
            print_acp_status(config);
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

    let agents_dir = std::env::var("REMI_AGENTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| data_dir.join("agents"));
    let models_dir = data_dir.join("models");
    let agent_registry = AgentRegistry::load(&agents_dir)?;
    let model_registry = ModelProfileRegistry::load(&models_dir)?;
    let api_key_present = match &setup_state {
        SetupState::Initialized { config, .. } => model_registry
            .get(&config.model_profile)
            .map(|profile| api_key_from_env(profile).is_ok())
            .unwrap_or(false),
        _ => model_registry
            .default_profile()
            .map(|profile| api_key_from_env(profile).is_ok())
            .unwrap_or(false),
    };
    println!(
        "api_key: {}",
        if api_key_present {
            "present"
        } else {
            "missing"
        }
    );
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
        }
    }
    Ok(())
}

pub(crate) async fn run_codex_command(
    profile: &InstanceProfile,
    data_dir: &Path,
    command: CodexCommand,
) -> anyhow::Result<()> {
    match command {
        CodexCommand::Setup { bin, agent, args } => {
            run_acp_command(
                profile,
                data_dir,
                AcpCommand::Setup {
                    client: "codex".to_string(),
                    mode: Some("local".to_string()),
                    tool_name: Some("codex".to_string()),
                    agent,
                    base_url: None,
                    model: None,
                    api_key: None,
                    bin,
                    args,
                },
            )
            .await
        }
        CodexCommand::Doctor => run_acp_doctor(profile, data_dir, "codex"),
    }
}

pub(crate) async fn run_acp_command(
    profile: &InstanceProfile,
    data_dir: &Path,
    command: AcpCommand,
) -> anyhow::Result<()> {
    match command {
        AcpCommand::Setup {
            client,
            mode,
            tool_name,
            agent,
            base_url,
            model,
            api_key,
            bin,
            args,
        } => {
            run_acp_setup(
                profile, data_dir, client, mode, tool_name, agent, base_url, model, api_key, bin,
                args,
            )
            .await
        }
        AcpCommand::Doctor => run_acp_doctor(profile, data_dir, "acp"),
        AcpCommand::Agent => {
            anyhow::bail!("`remi-cat acp agent` must be run as a top-level command")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_acp_setup(
    profile: &InstanceProfile,
    data_dir: &Path,
    client: String,
    mode: Option<String>,
    tool_name: Option<String>,
    agent: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    bin: Option<String>,
    args: Vec<String>,
) -> anyhow::Result<()> {
    let client = normalize_acp_client(&client)?;
    let has_local_bin = bin
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let mode = mode
        .map(|value| normalize_acp_mode_arg(&value))
        .transpose()?
        .unwrap_or_else(|| {
            if base_url
                .as_deref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
            {
                "remote".to_string()
            } else {
                "local".to_string()
            }
        });
    let mut entries = vec![
        "admin.enabled=false".to_string(),
        "im.mode=disabled".to_string(),
        format!("acp.mode={mode}"),
        format!("acp.client={client}"),
    ];
    let tool_name = tool_name.or_else(|| {
        if client == "codex" {
            Some("codex".to_string())
        } else {
            None
        }
    });
    if let Some(tool_name) = tool_name
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        validate_tool_name(&tool_name)?;
        entries.push(format!("acp.tool_name={tool_name}"));
    }
    let bin = bin
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Some(agent) = agent
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        entries.push(format!("acp.agent_name={agent}"));
    }
    if let Some(base_url) = base_url
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
    {
        entries.push(format!("acp.base_url={base_url}"));
    }
    if let Some(model) = model
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        entries.push(format!("acp.model={model}"));
    }
    if let Some(api_key) = api_key
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        entries.push(format!("acp.api_key={api_key}"));
    }
    let mut args: Vec<String> = args
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    if client == "codex" && mode == "local" {
        entries.push(format!("acp.local_bin={}", current_remi_cat_bin()?));
        entries.push(format!(
            "acp.local_args={}",
            serde_json::to_string(&codex_adapter_args(bin, &args))?
        ));
        apply_runtime_config_entries(profile, data_dir, &entries, true).await?;
        println!();
        return run_acp_doctor(profile, data_dir, "acp");
    }
    if let Some(bin) = bin {
        entries.push(format!("acp.local_bin={bin}"));
    }
    if args.is_empty() && client == "remi" && has_local_bin {
        args = vec!["acp".to_string(), "agent".to_string()];
    }
    if !args.is_empty() {
        let encoded = serde_json::to_string(&args)?;
        entries.push(format!("acp.local_args={encoded}"));
    }
    apply_runtime_config_entries(profile, data_dir, &entries, true).await?;
    println!();
    run_acp_doctor(profile, data_dir, "acp")
}

fn current_remi_cat_bin() -> anyhow::Result<String> {
    Ok(std::env::current_exe()?
        .to_string_lossy()
        .trim()
        .to_string())
}

fn codex_adapter_args(codex_bin: Option<String>, codex_args: &[String]) -> Vec<String> {
    let mut args = vec!["acp-adapter".to_string(), "codex".to_string()];
    if let Some(bin) = codex_bin {
        args.push("--bin".to_string());
        args.push(bin);
    }
    for arg in codex_args {
        args.push("--arg".to_string());
        args.push(arg.clone());
    }
    args
}

fn run_acp_doctor(profile: &InstanceProfile, data_dir: &Path, label: &str) -> anyhow::Result<()> {
    println!("remi-cat {label} doctor");
    println!("profile: {}", profile.label());
    println!("data_dir: {}", data_dir.display());
    match detect_setup_state(data_dir) {
        SetupState::Initialized {
            config_path,
            config,
        } => {
            println!("runtime_config: {}", config_path.display());
            println!("setup: initialized");
            print_acp_status(&config);
        }
        SetupState::Invalid { config_path, error } => {
            println!("runtime_config: {}", config_path.display());
            println!("setup: invalid");
            println!("error: {error}");
        }
        SetupState::LegacyEnvCompatible { data_dir } => {
            println!(
                "runtime_config: {}",
                data_dir.join("runtime.yaml").display()
            );
            println!("setup: legacy-env-compatible (no runtime.yaml)");
            print_acp_env_status();
        }
        SetupState::Uninitialized { data_dir } => {
            println!(
                "runtime_config: {}",
                data_dir.join("runtime.yaml").display()
            );
            println!("setup: not initialized");
            print_acp_env_status();
        }
    }
    Ok(())
}

fn normalize_acp_client(value: &str) -> anyhow::Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("ACP client may only contain ASCII letters, digits, `-`, and `_`");
    }
    Ok(value)
}

fn normalize_acp_mode_arg(value: &str) -> anyhow::Result<String> {
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "local" | "stub" | "local_stub" => Ok("local".to_string()),
        "remote" => Ok("remote".to_string()),
        other => anyhow::bail!("unknown ACP mode `{other}`"),
    }
}

fn validate_tool_name(value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        anyhow::bail!("ACP tool name may only contain ASCII letters, digits, `_`, and `-`");
    }
    Ok(())
}

fn print_acp_status(config: &RuntimeConfig) {
    let client = config.acp.client.as_str();
    println!("acp_mode: {}", config.acp.mode.as_env_value());
    println!("acp_client: {client}");
    println!("acp_tool_name: {}", configured_acp_tool_name(config));
    println!(
        "acp_agent_name: {}",
        config.acp.agent_name.as_deref().unwrap_or("default")
    );
    if matches!(config.acp.client, AcpClient::Remi) {
        println!(
            "remi_mode: {}",
            if has_explicit_local_acp_bin(config) {
                "external_stdio"
            } else {
                "internal"
            }
        );
    }
    if matches!(config.acp.mode, AcpMode::Remote) {
        println!(
            "acp_base_url: {}",
            config.acp.base_url.as_deref().unwrap_or("unset")
        );
        println!(
            "acp_model: {}",
            config.acp.model.as_deref().unwrap_or("unset")
        );
        println!(
            "acp_api_key: {}",
            if config
                .acp
                .api_key
                .as_deref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
            {
                "set"
            } else {
                "unset"
            }
        );
    }
    let bin = configured_local_acp_bin(config);
    let binary_status = local_acp_binary_status(&bin);
    println!("acp_local_bin: {bin}");
    println!(
        "acp_local_args_count: {}",
        configured_local_args_count(config)
    );
    println!("acp_local_binary: {}", binary_status.label());
    println!("acp_tool: {}", acp_tool_status(config, &binary_status));
}

fn print_acp_env_status() {
    let client = std::env::var("REMI_ACP_CLIENT").unwrap_or_else(|_| "unset".to_string());
    let mode = std::env::var("REMI_ACP_MODE").unwrap_or_else(|_| "unset".to_string());
    let bin = configured_local_acp_bin_from_env();
    let binary_status = local_acp_binary_status(&bin);
    println!("acp_mode: {mode}");
    println!("acp_client: {client}");
    println!(
        "acp_tool_name: {}",
        std::env::var("REMI_ACP_TOOL_NAME").unwrap_or_else(|_| default_acp_tool_name(&client))
    );
    println!(
        "acp_agent_name: {}",
        std::env::var("REMI_ACP_AGENT_NAME").unwrap_or_else(|_| "default".to_string())
    );
    println!(
        "acp_base_url: {}",
        std::env::var("REMI_ACP_BASE_URL").unwrap_or_else(|_| "unset".to_string())
    );
    println!("acp_local_bin: {bin}");
    println!(
        "acp_local_args_count: {}",
        args_count_from_env("REMI_ACP_LOCAL_ARGS")
    );
    println!("acp_local_binary: {}", binary_status.label());
}

fn configured_acp_tool_name(config: &RuntimeConfig) -> String {
    config
        .acp
        .tool_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_acp_tool_name(config.acp.client.as_str()))
}

fn default_acp_tool_name(client: &str) -> String {
    format!("acp__{}", client.trim().replace('-', "_"))
}

fn configured_local_acp_bin(config: &RuntimeConfig) -> String {
    config
        .acp
        .local_bin
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("REMI_ACP_LOCAL_BIN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| config.acp.client.as_str().to_string())
}

fn has_explicit_local_acp_bin(config: &RuntimeConfig) -> bool {
    config
        .acp
        .local_bin
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

fn configured_local_acp_bin_from_env() -> String {
    let client = std::env::var("REMI_ACP_CLIENT").unwrap_or_else(|_| "acp".to_string());
    std::env::var("REMI_ACP_LOCAL_BIN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(client)
}

fn configured_local_args_count(config: &RuntimeConfig) -> usize {
    config.acp.local_args.len()
}

fn local_acp_binary_status(bin: &str) -> LocalBinaryStatus {
    local_binary_status(bin)
}

fn acp_tool_status(config: &RuntimeConfig, binary_status: &LocalBinaryStatus) -> &'static str {
    match config.acp.mode {
        AcpMode::Remote => {
            if config
                .acp
                .base_url
                .as_deref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
            {
                "available"
            } else {
                "not injected"
            }
        }
        AcpMode::LocalStub => match config.acp.client {
            AcpClient::Remi if !has_explicit_local_acp_bin(config) => "available",
            _ if matches!(binary_status, LocalBinaryStatus::Available) => "available",
            _ => "not injected",
        },
    }
}

fn args_count_from_env(key: &str) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| serde_json::from_str::<Vec<String>>(&value).ok())
        .map(|args| args.len())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalBinaryStatus {
    Available,
    Missing,
    Failed(String),
}

impl LocalBinaryStatus {
    fn label(&self) -> String {
        match self {
            Self::Available => "available".to_string(),
            Self::Missing => "missing".to_string(),
            Self::Failed(message) => format!("failed ({message})"),
        }
    }
}

fn local_binary_status(bin: &str) -> LocalBinaryStatus {
    match std::process::Command::new(bin).arg("--version").output() {
        Ok(output) if output.status.success() => LocalBinaryStatus::Available,
        Ok(output) => LocalBinaryStatus::Failed(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("version command failed")
                .trim()
                .to_string(),
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => LocalBinaryStatus::Missing,
        Err(err) => LocalBinaryStatus::Failed(err.to_string()),
    }
}

fn has_all_tools(tools: &[String], required: &[&str]) -> bool {
    required
        .iter()
        .all(|required| tools.iter().any(|tool| tool == required))
}

pub(crate) fn sandbox_doctor_report(data_dir: &Path, setup_state: &SetupState) -> String {
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

const REQUIRED_SANDBOX_COMMANDS: &[&str] = &["bash", "sleep", "cat", "rg"];
const RECOMMENDED_SANDBOX_COMMANDS: &[&str] = &[
    "git", "curl", "wget", "python3", "pip3", "jq", "gcc", "make", "tar", "gzip", "unzip", "zip",
    "sed", "grep", "awk", "find", "xargs", "wc", "head", "tail", "sort",
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

pub(crate) fn command_doctor_report(runtime: &Runtime) -> String {
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
    format!(
        "**remi-cat doctor**\n\n```text\nroot_agent_id: {}\ntool_todo: {}\n```",
        runtime.root_agent_id,
        if todo_enabled { "enabled" } else { "missing" },
    )
}

pub(crate) fn print_registered_tools(bot: &CatBot, json: bool) -> anyhow::Result<()> {
    let tools = bot.registered_tool_statuses();
    if json {
        println!("{}", serde_json::to_string_pretty(&tools)?);
        return Ok(());
    }

    for tool in tools {
        let status = if !tool.registered {
            "not_registered"
        } else if !tool.enabled {
            "disabled"
        } else {
            "enabled"
        };
        println!(
            "{}\t{}\tallowlist={}\t{}",
            tool.name,
            status,
            if tool.in_active_allowlist {
                "yes"
            } else {
                "no"
            },
            tool.description
        );
        for warning in tool.warnings {
            println!("  warning: {warning}");
        }
        for error in tool.errors {
            println!("  error: {error}");
        }
    }
    Ok(())
}
