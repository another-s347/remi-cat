use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{sandbox::SandboxConfig, MemoryStore, Message};

pub(super) const ENVIRONMENT_CONTEXT_STATE_KEY: &str = "__environment_context";

#[derive(Debug, Clone)]
pub(super) struct EnvironmentContextSource {
    operating_system: String,
    shell: String,
    local_shell_enabled: Option<bool>,
    current_directory: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct EnvironmentContext {
    version: u8,
    operating_system: String,
    shell: String,
    current_directory: String,
}

pub(super) struct EnsuredEnvironmentContext {
    pub(super) prompt: String,
    pub(super) initialized: bool,
}

impl EnvironmentContextSource {
    pub(super) fn from_sandbox_config(config: &SandboxConfig) -> Self {
        match config {
            SandboxConfig::Docker(config) => Self {
                operating_system: format!("Linux (Docker image: {})", config.image),
                shell: "bash".to_string(),
                local_shell_enabled: None,
                current_directory: config.container_dir.clone(),
            },
            SandboxConfig::NoSandbox { host_dir } => Self::local(host_dir, true),
            SandboxConfig::Disabled { host_dir } => Self::local(host_dir, false),
        }
    }

    fn local(host_dir: &Path, shell_enabled: bool) -> Self {
        Self {
            operating_system: local_operating_system(),
            shell: if shell_enabled {
                String::new()
            } else {
                "unavailable (shell execution disabled)".to_string()
            },
            local_shell_enabled: Some(shell_enabled),
            current_directory: host_dir.display().to_string(),
        }
    }

    fn capture(&self) -> EnvironmentContext {
        let shell = if self.local_shell_enabled == Some(true) {
            local_shell_name()
        } else {
            self.shell.clone()
        };
        EnvironmentContext {
            version: 3,
            operating_system: sanitize_value(&self.operating_system),
            shell: sanitize_value(&shell),
            current_directory: sanitize_value(&self.current_directory),
        }
    }
}

pub(super) fn ensure_environment_context(
    user_state: &mut serde_json::Value,
    source: &EnvironmentContextSource,
) -> EnsuredEnvironmentContext {
    if let Some(context) = user_state
        .get(ENVIRONMENT_CONTEXT_STATE_KEY)
        .and_then(|value| serde_json::from_value::<EnvironmentContext>(value.clone()).ok())
        .filter(|context| context.version == 3)
    {
        return EnsuredEnvironmentContext {
            prompt: render_environment_context(&context),
            initialized: false,
        };
    }

    if !user_state.is_object() {
        *user_state = serde_json::json!({});
    }
    let context = source.capture();
    if let Some(map) = user_state.as_object_mut() {
        map.insert(
            ENVIRONMENT_CONTEXT_STATE_KEY.to_string(),
            serde_json::to_value(&context).expect("environment context serializes"),
        );
    }
    EnsuredEnvironmentContext {
        prompt: render_environment_context(&context),
        initialized: true,
    }
}

pub(super) async fn persist_new_environment_context(
    memory: &MemoryStore,
    thread_id: &str,
    user_state: &serde_json::Value,
    initialized: bool,
) {
    if initialized {
        if let Err(error) = memory.save_user_state(thread_id, user_state).await {
            tracing::warn!(thread_id, error = %error, "environment context persistence failed");
        }
    }
}

pub(super) fn insert_environment_context_prompt(
    history: &mut Vec<Message>,
    insertion_index: usize,
    prompt: String,
) {
    history.insert(insertion_index.min(history.len()), Message::system(prompt));
}

pub(super) fn remove_environment_context(user_state: &mut serde_json::Value) {
    if let Some(map) = user_state.as_object_mut() {
        map.remove(ENVIRONMENT_CONTEXT_STATE_KEY);
    }
}

fn render_environment_context(context: &EnvironmentContext) -> String {
    format!(
        "## Environment Context\n\n- Operating system: {}\n- Shell: {}\n- Current directory: {}",
        context.operating_system, context.shell, context.current_directory
    )
}

fn sanitize_value(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            _ => character,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn local_shell_name() -> String {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "bash".to_string());
    let name = Path::new(&shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("bash");
    match name {
        "bash" | "zsh" => name.to_string(),
        _ => "bash".to_string(),
    }
}

fn local_operating_system() -> String {
    match std::env::consts::OS {
        "linux" => linux_pretty_name().unwrap_or_else(|| "Linux".to_string()),
        "macos" => command_version("sw_vers", &["-productVersion"])
            .map(|version| format!("macOS {version}"))
            .unwrap_or_else(|| "macOS".to_string()),
        "windows" => {
            command_version("cmd", &["/C", "ver"]).unwrap_or_else(|| "Windows".to_string())
        }
        other => other.to_string(),
    }
}

fn linux_pretty_name() -> Option<String> {
    let os_release = std::fs::read_to_string("/etc/os-release").ok()?;
    let pretty_name = os_release.lines().find_map(|line| {
        let value = line.strip_prefix("PRETTY_NAME=")?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.to_string())
    })?;
    let kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|value| sanitize_value(&value));
    Some(format_linux_operating_system(
        &pretty_name,
        kernel_release.as_deref(),
    ))
}

fn format_linux_operating_system(pretty_name: &str, kernel_release: Option<&str>) -> String {
    let Some(kernel_release) = kernel_release.filter(|value| !value.is_empty()) else {
        return pretty_name.to_string();
    };
    let normalized = kernel_release.to_ascii_lowercase();
    let wsl = if normalized.contains("wsl2") {
        Some("WSL2")
    } else if normalized.contains("microsoft") {
        Some("WSL1")
    } else {
        None
    };
    match wsl {
        Some(wsl) => format!("{pretty_name} ({wsl}, Linux {kernel_release})"),
        None => pretty_name.to_string(),
    }
}

fn command_version(command: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(command)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = sanitize_value(&value);
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_is_created_once_and_then_frozen() {
        let source = EnvironmentContextSource::local(Path::new("/first"), true);
        let mut state = serde_json::Value::Null;
        let first = ensure_environment_context(&mut state, &source);
        assert!(first.initialized);
        assert!(first.prompt.contains("- Current directory: /first"));

        let changed = EnvironmentContextSource::local(Path::new("/changed"), true);
        let second = ensure_environment_context(&mut state, &changed);
        assert!(!second.initialized);
        assert_eq!(second.prompt, first.prompt);
    }

    #[test]
    fn values_are_kept_on_one_line() {
        assert_eq!(
            sanitize_value("safe\n## injected\ttext"),
            "safe ## injected text"
        );
    }

    #[test]
    fn docker_context_uses_model_visible_execution_environment() {
        let source = EnvironmentContextSource::from_sandbox_config(&SandboxConfig::Docker(
            crate::sandbox::DockerSandboxConfig {
                host_dir: "/host/workspace".into(),
                container_dir: "/workspace".to_string(),
                image: "example/image:latest".to_string(),
                container_name: "test".to_string(),
                user: None,
            },
        ));
        let mut state = serde_json::Value::Null;
        let context = ensure_environment_context(&mut state, &source);

        assert!(context
            .prompt
            .contains("- Operating system: Linux (Docker image: example/image:latest)"));
        assert!(context.prompt.contains("- Shell: bash"));
        assert!(context.prompt.contains("- Current directory: /workspace"));
        assert!(!context.prompt.contains("/host/workspace"));
    }

    #[test]
    fn disabled_context_reports_unavailable_shell() {
        let source = EnvironmentContextSource::from_sandbox_config(&SandboxConfig::Disabled {
            host_dir: "/workspace".into(),
        });
        let mut state = serde_json::Value::Null;
        let context = ensure_environment_context(&mut state, &source);

        assert!(context
            .prompt
            .contains("- Shell: unavailable (shell execution disabled)"));
    }

    #[test]
    fn removing_context_makes_the_next_capture_fresh() {
        let mut state = serde_json::Value::Null;
        let first = EnvironmentContextSource::local(Path::new("/first"), true);
        ensure_environment_context(&mut state, &first);
        remove_environment_context(&mut state);

        let second = EnvironmentContextSource::local(Path::new("/second"), true);
        let captured = ensure_environment_context(&mut state, &second);
        assert!(captured.initialized);
        assert!(captured.prompt.contains("- Current directory: /second"));
    }

    #[test]
    fn older_context_is_recaptured_without_execution_environment() {
        let mut state = serde_json::json!({
            ENVIRONMENT_CONTEXT_STATE_KEY: {
                "version": 2,
                "operating_system": "Linux",
                "environment": "local (no sandbox)",
                "shell": "bash",
                "current_directory": "/old"
            }
        });
        let source = EnvironmentContextSource::local(Path::new("/new"), true);
        let captured = ensure_environment_context(&mut state, &source);

        assert!(captured.initialized);
        assert!(!captured.prompt.contains("Execution environment"));
        assert!(captured.prompt.contains("- Current directory: /new"));
    }

    #[test]
    fn linux_operating_system_identifies_wsl_versions() {
        assert_eq!(
            format_linux_operating_system(
                "Ubuntu 22.04.5 LTS",
                Some("6.18.33.2-microsoft-standard-WSL2")
            ),
            "Ubuntu 22.04.5 LTS (WSL2, Linux 6.18.33.2-microsoft-standard-WSL2)"
        );
        assert_eq!(
            format_linux_operating_system("Ubuntu 20.04 LTS", Some("4.4.0-19041-Microsoft")),
            "Ubuntu 20.04 LTS (WSL1, Linux 4.4.0-19041-Microsoft)"
        );
        assert_eq!(
            format_linux_operating_system("Debian GNU/Linux 12", Some("6.1.0-amd64")),
            "Debian GNU/Linux 12"
        );
    }
}
