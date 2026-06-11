use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const RUNTIME_FILE_NAME: &str = "runtime.yaml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub data_dir: String,
    pub root_agent_id: String,
    pub model_profile: String,
    #[serde(default)]
    pub sandbox: RuntimeSandboxConfig,
    #[serde(default)]
    pub admin: AdminConfig,
    #[serde(default)]
    pub im: ImConfig,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub acp: AcpConfig,
}

impl RuntimeConfig {
    pub fn default_for(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.display().to_string(),
            root_agent_id: "default".to_string(),
            model_profile: "default".to_string(),
            sandbox: RuntimeSandboxConfig::default_for(data_dir),
            admin: AdminConfig::default(),
            im: ImConfig::default(),
            shell: ShellConfig::default(),
            acp: AcpConfig::default(),
        }
    }

    pub fn apply_env_defaults(&self) {
        set_env_if_absent("REMI_DATA_DIR", &self.data_dir);
        set_env_if_absent("REMI_AGENT_ID", &self.root_agent_id);
        set_env_if_absent("REMI_MODEL_PROFILE", &self.model_profile);
        set_env_if_absent("REMI_SANDBOX_KIND", self.sandbox.kind.as_env_value());
        set_env_if_absent(
            "REMI_SANDBOX_HOST_DIR",
            &self.sandbox.host_dir_or_data_dir(&self.data_dir),
        );
        set_env_if_absent("REMI_SANDBOX_CONTAINER_DIR", &self.sandbox.container_dir);
        set_env_if_absent("REMI_SANDBOX_IMAGE", &self.sandbox.image);
        set_env_if_absent("REMI_SANDBOX_CONTAINER_NAME", &self.sandbox.container_name);
        set_env_if_absent(
            "REMI_ADMIN_ENABLED",
            if self.admin.enabled { "true" } else { "false" },
        );
        set_env_if_absent("REMI_ADMIN_HOST", &self.admin.host);
        set_env_if_absent("REMI_ADMIN_PORT", &self.admin.port.to_string());
        set_env_if_absent("REMI_FEISHU_TRANSPORT", self.im.transport.as_env_value());
        set_env_if_absent("REMI_FEISHU_HOOK_HOST", &self.im.event_hook.host);
        set_env_if_absent(
            "REMI_FEISHU_HOOK_PORT",
            &self.im.event_hook.port.to_string(),
        );
        set_env_if_absent("REMI_FEISHU_HOOK_PATH", &self.im.event_hook.path);
        if !self.im.event_hook.verification_token.trim().is_empty() {
            set_env_if_absent(
                "REMI_FEISHU_HOOK_VERIFICATION_TOKEN",
                &self.im.event_hook.verification_token,
            );
        }
        set_env_if_absent("REMI_SHELL_MODE", self.shell.mode.as_env_value());
        set_env_if_absent("REMI_ACP_MODE", self.acp.mode.as_env_value());
        set_env_if_absent("REMI_ACP_CLIENT", self.acp.client.as_env_value());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeSandboxConfig {
    pub kind: RuntimeSandboxKind,
    #[serde(default)]
    pub host_dir: String,
    #[serde(default = "default_container_dir")]
    pub container_dir: String,
    #[serde(default = "default_sandbox_image")]
    pub image: String,
    #[serde(default = "default_container_name")]
    pub container_name: String,
}

impl RuntimeSandboxConfig {
    pub fn default_for(data_dir: &Path) -> Self {
        Self {
            kind: RuntimeSandboxKind::NoSandbox,
            host_dir: data_dir.display().to_string(),
            container_dir: default_container_dir(),
            image: default_sandbox_image(),
            container_name: default_container_name(),
        }
    }

    pub fn host_dir_or_data_dir(&self, data_dir: &str) -> String {
        if self.host_dir.trim().is_empty() {
            data_dir.to_string()
        } else {
            self.host_dir.clone()
        }
    }
}

impl Default for RuntimeSandboxConfig {
    fn default() -> Self {
        Self {
            kind: RuntimeSandboxKind::Disabled,
            host_dir: String::new(),
            container_dir: default_container_dir(),
            image: default_sandbox_image(),
            container_name: default_container_name(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSandboxKind {
    Disabled,
    NoSandbox,
    Docker,
}

impl RuntimeSandboxKind {
    pub fn as_env_value(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::NoSandbox => "no_sandbox",
            Self::Docker => "docker",
        }
    }
}

fn default_container_dir() -> String {
    "/workspace".to_string()
}

fn default_sandbox_image() -> String {
    "mcr.microsoft.com/devcontainers/base:bookworm".to_string()
}

fn default_container_name() -> String {
    "remi-cat-sandbox".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdminConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 8787,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImConfig {
    pub mode: ImMode,
    #[serde(default)]
    pub transport: FeishuTransport,
    #[serde(default)]
    pub event_hook: FeishuEventHookRuntimeConfig,
}

impl Default for ImConfig {
    fn default() -> Self {
        Self {
            mode: ImMode::Feishu,
            transport: FeishuTransport::WebSocket,
            event_hook: FeishuEventHookRuntimeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImMode {
    Feishu,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FeishuTransport {
    #[default]
    WebSocket,
    EventHook,
}

impl FeishuTransport {
    pub fn as_env_value(&self) -> &'static str {
        match self {
            Self::WebSocket => "websocket",
            Self::EventHook => "event_hook",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeishuEventHookRuntimeConfig {
    pub host: String,
    pub port: u16,
    pub path: String,
    #[serde(default)]
    pub verification_token: String,
}

impl Default for FeishuEventHookRuntimeConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8788,
            path: "/feishu/events".to_string(),
            verification_token: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellConfig {
    pub mode: ShellMode,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            mode: ShellMode::Disabled,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShellMode {
    Disabled,
    Local,
}

impl ShellMode {
    pub fn as_env_value(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpConfig {
    pub mode: AcpMode,
    #[serde(default)]
    pub client: AcpClient,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            mode: AcpMode::LocalStub,
            client: AcpClient::Codex,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AcpClient {
    #[default]
    Remi,
    Codex,
}

impl AcpClient {
    pub fn as_env_value(&self) -> &'static str {
        match self {
            Self::Remi => "remi",
            Self::Codex => "codex",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AcpMode {
    LocalStub,
    Remote,
}

impl AcpMode {
    pub fn as_env_value(&self) -> &'static str {
        match self {
            Self::LocalStub => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupState {
    Initialized {
        config_path: PathBuf,
        config: RuntimeConfig,
    },
    Invalid {
        config_path: PathBuf,
        error: String,
    },
    LegacyEnvCompatible {
        data_dir: PathBuf,
    },
    Uninitialized {
        data_dir: PathBuf,
    },
}

impl SetupState {
    pub fn is_initialized(&self) -> bool {
        matches!(self, Self::Initialized { .. })
    }

    pub fn config_path(&self) -> PathBuf {
        match self {
            Self::Initialized { config_path, .. } | Self::Invalid { config_path, .. } => {
                config_path.clone()
            }
            Self::LegacyEnvCompatible { data_dir } | Self::Uninitialized { data_dir } => {
                runtime_config_path(data_dir)
            }
        }
    }
}

pub fn runtime_config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RUNTIME_FILE_NAME)
}

pub fn load_runtime_config(data_dir: &Path) -> Result<Option<RuntimeConfig>> {
    let path = runtime_config_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config =
        serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(config))
}

pub fn write_runtime_config(data_dir: &Path, config: &RuntimeConfig) -> Result<PathBuf> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let path = runtime_config_path(data_dir);
    let raw = serde_yaml::to_string(config).context("serializing runtime config")?;
    std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub fn detect_setup_state(data_dir: &Path) -> SetupState {
    let path = runtime_config_path(data_dir);
    if path.exists() {
        match load_runtime_config(data_dir) {
            Ok(Some(config)) => SetupState::Initialized {
                config_path: path,
                config,
            },
            Ok(None) => SetupState::Uninitialized {
                data_dir: data_dir.to_path_buf(),
            },
            Err(err) => SetupState::Invalid {
                config_path: path,
                error: err.to_string(),
            },
        }
    } else if has_legacy_env_credentials() {
        SetupState::LegacyEnvCompatible {
            data_dir: data_dir.to_path_buf(),
        }
    } else {
        SetupState::Uninitialized {
            data_dir: data_dir.to_path_buf(),
        }
    }
}

pub fn has_legacy_env_credentials() -> bool {
    std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("REMI_API_KEY"))
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

pub fn load_dotenv_pairs(path: &Path) -> Result<BTreeMap<String, String>> {
    let mut pairs = BTreeMap::new();
    if !path.exists() {
        return Ok(pairs);
    }
    for item in
        dotenvy::from_path_iter(path).with_context(|| format!("reading {}", path.display()))?
    {
        let (key, value) = item?;
        pairs.insert(key, value);
    }
    Ok(pairs)
}

pub fn write_dotenv_pairs(path: &Path, pairs: &BTreeMap<String, String>) -> Result<()> {
    let mut lines = String::new();
    for (key, value) in pairs {
        lines.push_str(key);
        lines.push('=');
        lines.push_str(value);
        lines.push('\n');
    }
    std::fs::write(path, lines).with_context(|| format!("writing {}", path.display()))
}

pub fn upsert_dotenv_value(path: &Path, key: &str, value: &str) -> Result<()> {
    let mut pairs = load_dotenv_pairs(path)?;
    pairs.insert(key.to_string(), value.to_string());
    write_dotenv_pairs(path, &pairs)
}

fn set_env_if_absent(key: &str, value: &str) {
    if std::env::var_os(key).is_none() {
        unsafe {
            std::env::set_var(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        detect_setup_state, load_runtime_config, runtime_config_path, write_runtime_config,
        AcpClient, AdminConfig, RuntimeConfig, SetupState,
    };

    #[test]
    fn runtime_config_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("remi-runtime-config-{}", uuid::Uuid::new_v4()));
        let cfg = RuntimeConfig {
            data_dir: dir.display().to_string(),
            root_agent_id: "default".into(),
            model_profile: "deepseek-v4-flash".into(),
            sandbox: super::RuntimeSandboxConfig::default_for(&dir),
            admin: AdminConfig::default(),
            im: Default::default(),
            shell: Default::default(),
            acp: Default::default(),
        };
        write_runtime_config(&dir, &cfg).unwrap();
        let loaded = load_runtime_config(&dir).unwrap().unwrap();
        assert_eq!(loaded, cfg);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_invalid_runtime_config() {
        let dir =
            std::env::temp_dir().join(format!("remi-runtime-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(runtime_config_path(&dir), "not: [valid").unwrap();
        match detect_setup_state(&dir) {
            SetupState::Invalid { .. } => {}
            other => panic!("expected invalid state, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_legacy_env_compatibility_without_runtime_yaml() {
        let dir =
            std::env::temp_dir().join(format!("remi-runtime-config-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test-key");
        }
        match detect_setup_state(&dir) {
            SetupState::LegacyEnvCompatible { .. } => {}
            other => panic!("expected legacy env state, got {other:?}"),
        }
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    #[test]
    fn default_acp_client_is_codex() {
        let cfg = RuntimeConfig::default_for(std::path::Path::new(".remi-cat"));
        assert_eq!(cfg.acp.client, AcpClient::Codex);
    }
}
