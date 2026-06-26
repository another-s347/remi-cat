use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use figment::{
    providers::{Format, Serialized, Yaml},
    Figment,
};
use serde::{Deserialize, Serialize};

const RUNTIME_FILE_NAME: &str = "runtime.yaml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub data_dir: String,
    pub root_agent_id: String,
    pub model_profile: String,
    #[serde(default)]
    pub tool_output: ToolOutputConfig,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeConfigResolutionSource {
    Initialized,
    LegacyTuiDefaults,
    Unresolved,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfigResolution {
    pub data_dir: PathBuf,
    pub config: Option<RuntimeConfig>,
    pub source: RuntimeConfigResolutionSource,
}

impl RuntimeConfig {
    pub fn default_for(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.display().to_string(),
            root_agent_id: "default".to_string(),
            model_profile: "default".to_string(),
            tool_output: ToolOutputConfig::default(),
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
        if let Some(overflow_bytes) = self.tool_output.overflow_bytes {
            set_env_if_absent(
                "REMI_TOOL_OUTPUT_OVERFLOW_BYTES",
                &overflow_bytes.to_string(),
            );
        }
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
        set_env_if_absent("REMI_IM_MODE", self.im.mode.as_env_value());
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
        set_env_if_absent("REMI_ACP_CLIENT", self.acp.client.as_str());
        if let Some(tool_name) = self
            .acp
            .tool_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            set_env_if_absent("REMI_ACP_TOOL_NAME", tool_name);
        }
        if let Some(base_url) = self
            .acp
            .base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            set_env_if_absent("REMI_ACP_BASE_URL", base_url);
        }
        if let Some(model) = self
            .acp
            .model
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            set_env_if_absent("REMI_ACP_MODEL", model);
        }
        if let Some(api_key) = self
            .acp
            .api_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            set_env_if_absent("REMI_ACP_API_KEY", api_key);
        }
        if let Some(agent_name) = self
            .acp
            .agent_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            set_env_if_absent("REMI_ACP_AGENT_NAME", agent_name);
        }
        if let Some(local_bin) = self
            .acp
            .local_bin
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            set_env_if_absent("REMI_ACP_LOCAL_BIN", local_bin);
        }
        if !self.acp.local_args.is_empty() {
            match serde_json::to_string(&self.acp.local_args) {
                Ok(value) => set_env_if_absent("REMI_ACP_LOCAL_ARGS", &value),
                Err(err) => {
                    tracing::warn!(error = %err, "failed to serialize ACP local startup args")
                }
            }
        }
        if let Some(codex_bin) = self
            .acp
            .codex_bin
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            set_env_if_absent("REMI_ACP_CODEX_BIN", codex_bin);
        }
        if !self.acp.codex_args.is_empty() {
            match serde_json::to_string(&self.acp.codex_args) {
                Ok(value) => set_env_if_absent("REMI_ACP_CODEX_ARGS", &value),
                Err(err) => {
                    tracing::warn!(error = %err, "failed to serialize Codex startup args")
                }
            }
        }
    }

    pub fn apply_env_overrides(&self) {
        self.apply_env_defaults();
        set_env("REMI_DATA_DIR", &self.data_dir);
        set_env("REMI_AGENT_ID", &self.root_agent_id);
        set_env("REMI_MODEL_PROFILE", &self.model_profile);
        if let Some(overflow_bytes) = self.tool_output.overflow_bytes {
            set_env(
                "REMI_TOOL_OUTPUT_OVERFLOW_BYTES",
                &overflow_bytes.to_string(),
            );
        } else {
            remove_env("REMI_TOOL_OUTPUT_OVERFLOW_BYTES");
        }
        set_env("REMI_SANDBOX_KIND", self.sandbox.kind.as_env_value());
        set_env(
            "REMI_SANDBOX_HOST_DIR",
            &self.sandbox.host_dir_or_data_dir(&self.data_dir),
        );
        set_env("REMI_SANDBOX_CONTAINER_DIR", &self.sandbox.container_dir);
        set_env("REMI_SANDBOX_IMAGE", &self.sandbox.image);
        set_env("REMI_SANDBOX_CONTAINER_NAME", &self.sandbox.container_name);
        set_env(
            "REMI_ADMIN_ENABLED",
            if self.admin.enabled { "true" } else { "false" },
        );
        set_env("REMI_ADMIN_HOST", &self.admin.host);
        set_env("REMI_ADMIN_PORT", &self.admin.port.to_string());
        set_env("REMI_IM_MODE", self.im.mode.as_env_value());
        set_env("REMI_FEISHU_TRANSPORT", self.im.transport.as_env_value());
        set_env("REMI_FEISHU_HOOK_HOST", &self.im.event_hook.host);
        set_env(
            "REMI_FEISHU_HOOK_PORT",
            &self.im.event_hook.port.to_string(),
        );
        set_env("REMI_FEISHU_HOOK_PATH", &self.im.event_hook.path);
        set_env_optional(
            "REMI_FEISHU_HOOK_VERIFICATION_TOKEN",
            nonempty_str(&self.im.event_hook.verification_token),
        );
        set_env("REMI_SHELL_MODE", self.shell.mode.as_env_value());
        set_env("REMI_ACP_MODE", self.acp.mode.as_env_value());
        set_env("REMI_ACP_CLIENT", self.acp.client.as_str());
        set_env_optional("REMI_ACP_TOOL_NAME", self.acp.tool_name.as_deref());
        set_env_optional("REMI_ACP_BASE_URL", self.acp.base_url.as_deref());
        set_env_optional("REMI_ACP_MODEL", self.acp.model.as_deref());
        set_env_optional("REMI_ACP_API_KEY", self.acp.api_key.as_deref());
        set_env_optional("REMI_ACP_AGENT_NAME", self.acp.agent_name.as_deref());
        set_env_optional("REMI_ACP_LOCAL_BIN", self.acp.local_bin.as_deref());
        set_env_json_array("REMI_ACP_LOCAL_ARGS", &self.acp.local_args);
        set_env_optional("REMI_ACP_CODEX_BIN", self.acp.codex_bin.as_deref());
        set_env_json_array("REMI_ACP_CODEX_ARGS", &self.acp.codex_args);
    }
}

pub fn resolve_runtime_config_for_run(
    data_dir: &Path,
    tui_mode: bool,
) -> Result<RuntimeConfigResolution> {
    match detect_setup_state(data_dir) {
        SetupState::Initialized { config, .. } => {
            config.apply_env_overrides();
            let data_dir = PathBuf::from(
                std::env::var("REMI_DATA_DIR").unwrap_or_else(|_| config.data_dir.clone()),
            );
            Ok(RuntimeConfigResolution {
                data_dir,
                config: Some(config),
                source: RuntimeConfigResolutionSource::Initialized,
            })
        }
        SetupState::LegacyEnvCompatible { .. } if tui_mode => {
            let config = RuntimeConfig::default_for(data_dir);
            config.apply_env_defaults();
            let data_dir = PathBuf::from(
                std::env::var("REMI_DATA_DIR").unwrap_or_else(|_| config.data_dir.clone()),
            );
            Ok(RuntimeConfigResolution {
                data_dir,
                config: Some(config),
                source: RuntimeConfigResolutionSource::LegacyTuiDefaults,
            })
        }
        SetupState::Invalid { error, .. } => anyhow::bail!("runtime config is invalid: {error}"),
        SetupState::LegacyEnvCompatible { .. } | SetupState::Uninitialized { .. } => {
            Ok(RuntimeConfigResolution {
                data_dir: data_dir.to_path_buf(),
                config: None,
                source: RuntimeConfigResolutionSource::Unresolved,
            })
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolOutputConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_bytes: Option<usize>,
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
    #[serde(default)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImMode {
    #[default]
    Feishu,
    Disabled,
}

impl ImMode {
    pub fn as_env_value(&self) -> &'static str {
        match self {
            Self::Feishu => "feishu",
            Self::Disabled => "disabled",
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_bin: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_bin: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codex_args: Vec<String>,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            mode: AcpMode::LocalStub,
            client: AcpClient::Codex,
            tool_name: None,
            agent_name: None,
            base_url: None,
            model: None,
            api_key: None,
            local_bin: None,
            local_args: Vec::new(),
            codex_bin: None,
            codex_args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpClient {
    Remi,
    Codex,
    Other(String),
}

impl Default for AcpClient {
    fn default() -> Self {
        Self::Codex
    }
}

impl AcpClient {
    pub fn new(value: impl Into<String>) -> Self {
        match value.into().trim().to_ascii_lowercase().as_str() {
            "remi" => Self::Remi,
            "codex" => Self::Codex,
            other => Self::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Remi => "remi",
            Self::Codex => "codex",
            Self::Other(value) => value,
        }
    }
}

impl Serialize for AcpClient {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AcpClient {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(serde::de::Error::custom("acp.client must not be empty"));
        }
        Ok(Self::new(trimmed))
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
    Figment::from(Serialized::defaults(RuntimeConfig::default_for(data_dir)))
        .merge(Yaml::file(&path))
        .extract()
        .with_context(|| format!("loading runtime config {}", path.display()))
        .map(Some)
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
    std::env::var("MIMO_API_KEY")
        .or_else(|_| std::env::var("MOONSHOT_API_KEY"))
        .or_else(|_| std::env::var("KIMI_API_KEY"))
        .or_else(|_| std::env::var("GLM_API_KEY"))
        .or_else(|_| std::env::var("ZHIPU_API_KEY"))
        .or_else(|_| std::env::var("BIGMODEL_API_KEY"))
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
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
        set_env(key, value);
    }
}

fn set_env(key: &str, value: &str) {
    unsafe {
        std::env::set_var(key, value);
    }
}

fn remove_env(key: &str) {
    unsafe {
        std::env::remove_var(key);
    }
}

fn nonempty_str(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

fn set_env_optional(key: &str, value: Option<&str>) {
    match value.filter(|value| !value.trim().is_empty()) {
        Some(value) => set_env(key, value),
        None => remove_env(key),
    }
}

fn set_env_json_array(key: &str, values: &[String]) {
    if values.is_empty() {
        remove_env(key);
        return;
    }
    match serde_json::to_string(values) {
        Ok(value) => set_env(key, &value),
        Err(err) => tracing::warn!(key, error = %err, "failed to serialize runtime env array"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        detect_setup_state, load_runtime_config, resolve_runtime_config_for_run,
        runtime_config_path, write_runtime_config, AcpClient, AdminConfig, ImMode, RuntimeConfig,
        RuntimeConfigResolutionSource, SetupState,
    };
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn runtime_config_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("remi-runtime-config-{}", uuid::Uuid::new_v4()));
        let mut cfg = RuntimeConfig {
            data_dir: dir.display().to_string(),
            root_agent_id: "default".into(),
            model_profile: "deepseek-v4-flash".into(),
            tool_output: super::ToolOutputConfig {
                overflow_bytes: Some(32_768),
            },
            sandbox: super::RuntimeSandboxConfig::default_for(&dir),
            admin: AdminConfig::default(),
            im: Default::default(),
            shell: Default::default(),
            acp: Default::default(),
        };
        cfg.acp.codex_args = vec!["--config".into(), "model=\"gpt-5-codex\"".into()];
        write_runtime_config(&dir, &cfg).unwrap();
        let loaded = load_runtime_config(&dir).unwrap().unwrap();
        assert_eq!(loaded, cfg);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_config_defaults_missing_tool_output() {
        let raw = r#"
data_dir: .remi-cat
root_agent_id: default
model_profile: default
"#;
        let cfg: RuntimeConfig = serde_yaml::from_str(raw).unwrap();

        assert_eq!(cfg.tool_output.overflow_bytes, None);
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
        let _guard = ENV_LOCK.lock().unwrap();
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

    #[test]
    fn apply_env_defaults_sets_codex_args_as_json() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("REMI_ACP_CODEX_ARGS");
        }
        let mut cfg = RuntimeConfig::default_for(std::path::Path::new(".remi-cat"));
        cfg.acp.codex_args = vec!["--config".into(), "model=\"gpt-5-codex\"".into()];
        cfg.apply_env_defaults();
        assert_eq!(
            std::env::var("REMI_ACP_CODEX_ARGS").unwrap(),
            r#"["--config","model=\"gpt-5-codex\""]"#
        );
        unsafe {
            std::env::remove_var("REMI_ACP_CODEX_ARGS");
        }
    }

    #[test]
    fn apply_env_overrides_replaces_stale_model_profile_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("REMI_MODEL_PROFILE", "default");
            std::env::set_var("REMI_AGENT_ID", "old-agent");
        }
        let mut cfg = RuntimeConfig::default_for(std::path::Path::new(".remi-cat"));
        cfg.root_agent_id = "default".to_string();
        cfg.model_profile = "deepseek-v4-flash".to_string();
        cfg.apply_env_overrides();
        assert_eq!(
            std::env::var("REMI_MODEL_PROFILE").unwrap(),
            "deepseek-v4-flash"
        );
        assert_eq!(std::env::var("REMI_AGENT_ID").unwrap(), "default");
        unsafe {
            std::env::remove_var("REMI_MODEL_PROFILE");
            std::env::remove_var("REMI_AGENT_ID");
        }
    }

    #[test]
    fn apply_env_overrides_replaces_stale_runtime_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("REMI_ACP_CLIENT", "codex");
            std::env::set_var("REMI_ACP_TOOL_NAME", "old_acp");
            std::env::set_var("REMI_ACP_LOCAL_ARGS", r#"["old"]"#);
            std::env::set_var("REMI_ACP_CODEX_ARGS", r#"["old"]"#);
            std::env::set_var("REMI_SHELL_MODE", "disabled");
            std::env::set_var("REMI_SANDBOX_KIND", "docker");
            std::env::set_var("REMI_IM_MODE", "feishu");
            std::env::set_var("REMI_TOOL_OUTPUT_OVERFLOW_BYTES", "1");
        }

        let mut cfg = RuntimeConfig::default_for(std::path::Path::new(".remi-cat"));
        cfg.tool_output.overflow_bytes = Some(7777);
        cfg.sandbox.kind = super::RuntimeSandboxKind::NoSandbox;
        cfg.shell.mode = super::ShellMode::Local;
        cfg.im.mode = ImMode::Disabled;
        cfg.acp.client = AcpClient::Remi;
        cfg.acp.tool_name = Some("e2e_acp".to_string());
        cfg.acp.local_args = vec!["hello".to_string(), "world".to_string()];
        cfg.acp.codex_args.clear();

        cfg.apply_env_overrides();

        assert_eq!(std::env::var("REMI_ACP_CLIENT").unwrap(), "remi");
        assert_eq!(std::env::var("REMI_ACP_TOOL_NAME").unwrap(), "e2e_acp");
        assert_eq!(
            std::env::var("REMI_ACP_LOCAL_ARGS").unwrap(),
            r#"["hello","world"]"#
        );
        assert!(std::env::var("REMI_ACP_CODEX_ARGS").is_err());
        assert_eq!(std::env::var("REMI_SHELL_MODE").unwrap(), "local");
        assert_eq!(std::env::var("REMI_SANDBOX_KIND").unwrap(), "no_sandbox");
        assert_eq!(std::env::var("REMI_IM_MODE").unwrap(), "disabled");
        assert_eq!(
            std::env::var("REMI_TOOL_OUTPUT_OVERFLOW_BYTES").unwrap(),
            "7777"
        );

        unsafe {
            std::env::remove_var("REMI_ACP_CLIENT");
            std::env::remove_var("REMI_ACP_TOOL_NAME");
            std::env::remove_var("REMI_ACP_LOCAL_ARGS");
            std::env::remove_var("REMI_ACP_CODEX_ARGS");
            std::env::remove_var("REMI_SHELL_MODE");
            std::env::remove_var("REMI_SANDBOX_KIND");
            std::env::remove_var("REMI_IM_MODE");
            std::env::remove_var("REMI_TOOL_OUTPUT_OVERFLOW_BYTES");
        }
    }

    #[test]
    fn resolver_uses_initialized_runtime_config_as_authority() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("remi-runtime-config-{}", uuid::Uuid::new_v4()));
        let mut cfg = RuntimeConfig::default_for(&dir);
        cfg.model_profile = "deepseek-v4-flash".to_string();
        write_runtime_config(&dir, &cfg).unwrap();
        unsafe {
            std::env::set_var("REMI_MODEL_PROFILE", "default");
        }

        let resolution = resolve_runtime_config_for_run(&dir, false).unwrap();
        assert_eq!(
            resolution.source,
            RuntimeConfigResolutionSource::Initialized
        );
        assert_eq!(resolution.data_dir, dir);
        assert_eq!(
            resolution.config.unwrap().model_profile,
            "deepseek-v4-flash"
        );
        assert_eq!(
            std::env::var("REMI_MODEL_PROFILE").unwrap(),
            "deepseek-v4-flash"
        );

        unsafe {
            std::env::remove_var("REMI_MODEL_PROFILE");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resolver_applies_legacy_defaults_only_for_tui_mode() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("remi-runtime-config-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::remove_var("REMI_DATA_DIR");
            std::env::remove_var("REMI_MODEL_PROFILE");
            std::env::set_var("OPENAI_API_KEY", "test-key");
        }

        let unresolved = resolve_runtime_config_for_run(&dir, false).unwrap();
        assert_eq!(unresolved.source, RuntimeConfigResolutionSource::Unresolved);
        assert!(unresolved.config.is_none());

        let tui_resolution = resolve_runtime_config_for_run(&dir, true).unwrap();
        assert_eq!(
            tui_resolution.source,
            RuntimeConfigResolutionSource::LegacyTuiDefaults
        );
        assert_eq!(
            std::env::var("REMI_MODEL_PROFILE").unwrap(),
            RuntimeConfig::default_for(&dir).model_profile
        );

        unsafe {
            std::env::remove_var("REMI_DATA_DIR");
            std::env::remove_var("REMI_MODEL_PROFILE");
            std::env::remove_var("OPENAI_API_KEY");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn disabled_im_mode_round_trips() {
        let dir =
            std::env::temp_dir().join(format!("remi-runtime-config-{}", uuid::Uuid::new_v4()));
        let mut cfg = RuntimeConfig::default_for(&dir);
        cfg.im.mode = ImMode::Disabled;
        write_runtime_config(&dir, &cfg).unwrap();
        let loaded = load_runtime_config(&dir).unwrap().unwrap();
        assert_eq!(loaded.im.mode, ImMode::Disabled);
        assert_eq!(loaded.im.mode.as_env_value(), "disabled");
        let _ = std::fs::remove_dir_all(dir);
    }
}
