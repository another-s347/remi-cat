use std::path::{Path, PathBuf};

use crate::cli::CodexCommand;
use crate::config::{
    detect_setup_state, AcpClient, AcpMode, FeishuTransport, RuntimeConfig, RuntimeSandboxKind,
    SetupState,
};
use crate::core::Runtime;
use crate::instance_profile::InstanceProfile;
use crate::profile_command::apply_runtime_config_entries;
use bot_core::{AgentRegistry, CatBot, ModelProfileRegistry};

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
            print_codex_status(config);
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

pub(crate) fn run_codex_command(
    profile: &InstanceProfile,
    data_dir: &Path,
    command: CodexCommand,
) -> anyhow::Result<()> {
    match command {
        CodexCommand::Setup { bin, agent, args } => {
            run_codex_setup(profile, data_dir, bin, agent, args)
        }
        CodexCommand::Doctor => run_codex_doctor(profile, data_dir),
    }
}

fn run_codex_setup(
    profile: &InstanceProfile,
    data_dir: &Path,
    bin: Option<String>,
    agent: Option<String>,
    args: Vec<String>,
) -> anyhow::Result<()> {
    let mut entries = vec![
        "admin.enabled=false".to_string(),
        "im.mode=disabled".to_string(),
        "acp.mode=local".to_string(),
        "acp.client=codex".to_string(),
    ];
    if let Some(bin) = bin
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        entries.push(format!("acp.codex_bin={bin}"));
    }
    if let Some(agent) = agent
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        entries.push(format!("acp.agent_name={agent}"));
    }
    let args: Vec<String> = args
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    if !args.is_empty() {
        entries.push(format!("acp.codex_args={}", serde_json::to_string(&args)?));
    }
    apply_runtime_config_entries(profile, data_dir, &entries, true)?;
    println!();
    run_codex_doctor(profile, data_dir)
}

fn run_codex_doctor(profile: &InstanceProfile, data_dir: &Path) -> anyhow::Result<()> {
    println!("remi-cat codex doctor");
    println!("profile: {}", profile.label());
    println!("data_dir: {}", data_dir.display());
    match detect_setup_state(data_dir) {
        SetupState::Initialized {
            config_path,
            config,
        } => {
            println!("runtime_config: {}", config_path.display());
            println!("setup: initialized");
            print_codex_status(&config);
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
            print_codex_env_status();
        }
        SetupState::Uninitialized { data_dir } => {
            println!(
                "runtime_config: {}",
                data_dir.join("runtime.yaml").display()
            );
            println!("setup: not initialized");
            print_codex_env_status();
        }
    }
    Ok(())
}

fn print_codex_status(config: &RuntimeConfig) {
    let bin = configured_codex_bin(config);
    let binary_status = codex_binary_status(&bin);
    println!("acp_mode: {}", config.acp.mode.as_env_value());
    println!("acp_client: {}", config.acp.client.as_env_value());
    println!(
        "acp_agent_name: {}",
        config.acp.agent_name.as_deref().unwrap_or("default")
    );
    println!("codex_bin: {bin}");
    println!("codex_args_count: {}", config.acp.codex_args.len());
    println!("codex_binary: {}", binary_status.label());
    println!(
        "codex_tool: {}",
        if codex_tool_configured(config) && matches!(binary_status, CodexBinaryStatus::Available) {
            "available"
        } else {
            "not injected"
        }
    );
}

fn print_codex_env_status() {
    let bin = std::env::var("REMI_ACP_CODEX_BIN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "codex".to_string());
    let binary_status = codex_binary_status(&bin);
    let client = std::env::var("REMI_ACP_CLIENT").unwrap_or_else(|_| "unset".to_string());
    let mode = std::env::var("REMI_ACP_MODE").unwrap_or_else(|_| "unset".to_string());
    println!("acp_mode: {mode}");
    println!("acp_client: {client}");
    println!(
        "acp_agent_name: {}",
        std::env::var("REMI_ACP_AGENT_NAME").unwrap_or_else(|_| "default".to_string())
    );
    println!("codex_bin: {bin}");
    println!(
        "codex_args_count: {}",
        codex_args_count_from_env("REMI_ACP_CODEX_ARGS")
    );
    println!("codex_binary: {}", binary_status.label());
    println!(
        "codex_tool: {}",
        if client.trim().eq_ignore_ascii_case("codex")
            && matches!(binary_status, CodexBinaryStatus::Available)
        {
            "available"
        } else {
            "not injected"
        }
    );
}

fn codex_tool_configured(config: &RuntimeConfig) -> bool {
    matches!(config.acp.client, AcpClient::Codex) && matches!(config.acp.mode, AcpMode::LocalStub)
}

fn configured_codex_bin(config: &RuntimeConfig) -> String {
    config
        .acp
        .codex_bin
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("REMI_ACP_CODEX_BIN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "codex".to_string())
}

fn codex_args_count_from_env(key: &str) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| serde_json::from_str::<Vec<String>>(&value).ok())
        .map(|args| args.len())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexBinaryStatus {
    Available,
    Missing,
    Failed(String),
}

impl CodexBinaryStatus {
    fn label(&self) -> String {
        match self {
            Self::Available => "available".to_string(),
            Self::Missing => "missing".to_string(),
            Self::Failed(message) => format!("failed ({message})"),
        }
    }
}

fn codex_binary_status(bin: &str) -> CodexBinaryStatus {
    match std::process::Command::new(bin).arg("--version").output() {
        Ok(output) if output.status.success() => CodexBinaryStatus::Available,
        Ok(output) => CodexBinaryStatus::Failed(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("version command failed")
                .trim()
                .to_string(),
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => CodexBinaryStatus::Missing,
        Err(err) => CodexBinaryStatus::Failed(err.to_string()),
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

pub(crate) fn sdk_doctor_report(data_dir: &Path) -> String {
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

fn env_var_present(key: &str) -> bool {
    std::env::var(key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}
