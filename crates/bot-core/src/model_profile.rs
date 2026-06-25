use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_MODEL_PROFILE_ID: &str = "default";
const LEGACY_FALLBACK_CONTEXT_TOKENS: u32 = 32_768;
const LEGACY_FALLBACK_MAX_OUTPUT_TOKENS: u32 = 4_096;

const EMBEDDED_MODEL_PROFILES: &[(&str, &str)] = &[
    (
        "default.yaml",
        include_str!("../../../.remi-cat/models/default.yaml"),
    ),
    (
        "gpt-4o.yaml",
        include_str!("../../../.remi-cat/models/gpt-4o.yaml"),
    ),
    (
        "deepseek-v4-flash.yaml",
        include_str!("../../../.remi-cat/models/deepseek-v4-flash.yaml"),
    ),
    (
        "deepseek-v4-pro.yaml",
        include_str!("../../../.remi-cat/models/deepseek-v4-pro.yaml"),
    ),
    (
        "mimo-v2.5-pro.yaml",
        include_str!("../../../.remi-cat/models/mimo-v2.5-pro.yaml"),
    ),
    (
        "mimo-v2.5.yaml",
        include_str!("../../../.remi-cat/models/mimo-v2.5.yaml"),
    ),
    (
        "kimi-k2.7-code.yaml",
        include_str!("../../../.remi-cat/models/kimi-k2.7-code.yaml"),
    ),
    (
        "kimi-k2.6.yaml",
        include_str!("../../../.remi-cat/models/kimi-k2.6.yaml"),
    ),
    (
        "glm-5.2.yaml",
        include_str!("../../../.remi-cat/models/glm-5.2.yaml"),
    ),
    (
        "glm-5v-turbo.yaml",
        include_str!("../../../.remi-cat/models/glm-5v-turbo.yaml"),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    Auto,
    Enabled,
    Disabled,
}

impl ThinkingMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Self::Auto),
            "enabled" | "enable" | "on" | "true" | "1" => Some(Self::Enabled),
            "disabled" | "disable" | "off" | "false" | "0" => Some(Self::Disabled),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Enabled => "Enabled",
            Self::Disabled => "Disabled",
        }
    }

    fn request_type(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Enabled => Some("enabled"),
            Self::Disabled => Some("disabled"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Auto,
    None,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(alias = "xhigh", alias = "extra_high")]
    XHigh,
    #[serde(alias = "maximum")]
    Max,
}

impl ReasoningEffort {
    pub const VARIANTS: [Self; 8] = [
        Self::Auto,
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "" | "auto" => Some(Self::Auto),
            "none" | "off" | "disabled" | "disable" | "false" | "0" => Some(Self::None),
            "minimal" | "min" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x_high" | "extra_high" => Some(Self::XHigh),
            "max" | "maximum" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::None => "None",
            Self::Minimal => "Minimal",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::XHigh => "Extra high",
            Self::Max => "Max",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => "Use the model profile or provider default",
            Self::None => "Disable explicit reasoning parameters",
            Self::Minimal => "Lowest-cost reasoning",
            Self::Low => "Light reasoning",
            Self::Medium => "Balanced reasoning",
            Self::High => "High reasoning",
            Self::XHigh => "Extra-high reasoning",
            Self::Max => "Maximum reasoning",
        }
    }

    fn request_value(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::None => Some("none"),
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::XHigh => Some("xhigh"),
            Self::Max => Some("max"),
        }
    }

    fn thinking_mode(self) -> Option<ThinkingMode> {
        match self {
            Self::Auto => None,
            Self::None | Self::Minimal => Some(ThinkingMode::Disabled),
            Self::Low | Self::Medium | Self::High | Self::XHigh | Self::Max => {
                Some(ThinkingMode::Enabled)
            }
        }
    }

    fn deepseek_glm_effort(self) -> Option<&'static str> {
        match self {
            Self::Auto | Self::None | Self::Minimal => None,
            Self::Low | Self::Medium | Self::High => Some("high"),
            Self::XHigh | Self::Max => Some("max"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfileConfig {
    pub id: String,
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub thinking: Option<ThinkingMode>,
    #[serde(default)]
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_output_tokens: u32,
    pub context_tokens: u32,
    pub supports_images: bool,
    pub short_term_tokens: usize,
    pub overflow_bytes: usize,
    pub auto_compress: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub extra_options: serde_json::Map<String, serde_json::Value>,
}

impl ModelProfileConfig {
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("model profile id is required");
        }
        if self.name.trim().is_empty() {
            bail!("model profile {} is missing name", self.id);
        }
        if self.model.trim().is_empty() {
            bail!("model profile {} is missing model", self.id);
        }
        if self.max_output_tokens == 0 {
            bail!("model profile {} max_output_tokens must be > 0", self.id);
        }
        if self.context_tokens == 0 {
            bail!("model profile {} context_tokens must be > 0", self.id);
        }
        if self.short_term_tokens == 0 {
            bail!("model profile {} short_term_tokens must be > 0", self.id);
        }
        if self.overflow_bytes == 0 {
            bail!("model profile {} overflow_bytes must be > 0", self.id);
        }
        Ok(())
    }

    pub fn merged_extra_options(
        &self,
        thinking_override: Option<ThinkingMode>,
        reasoning_override: Option<ReasoningEffort>,
    ) -> Result<serde_json::Map<String, serde_json::Value>> {
        let mut options = self.extra_options.clone();
        let thinking = thinking_override.or(self.thinking);
        let reasoning_effort = reasoning_override.or(self.reasoning_effort);
        apply_reasoning_options(self, thinking, reasoning_effort, &mut options)?;
        Ok(options)
    }
}

fn apply_reasoning_options(
    profile: &ModelProfileConfig,
    thinking: Option<ThinkingMode>,
    effort: Option<ReasoningEffort>,
    options: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let Some((thinking, effort)) = merged_reasoning_controls(thinking, effort)? else {
        return Ok(());
    };

    let lower = profile.model.trim().to_ascii_lowercase();
    let provider = profile
        .provider
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let base_url = profile
        .base_url
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();

    if provider == "deepseek" || base_url.contains("deepseek.com") || lower.contains("deepseek") {
        apply_thinking_type(thinking_for_effort(thinking, effort)?, options);
        if let Some(effort_value) = effort.and_then(ReasoningEffort::deepseek_glm_effort) {
            options.insert("reasoning_effort".into(), serde_json::json!(effort_value));
        }
        return Ok(());
    }

    if provider == "glm"
        || provider == "zhipu"
        || provider == "bigmodel"
        || base_url.contains("bigmodel.cn")
        || lower.contains("glm")
    {
        apply_thinking_type(thinking_for_effort(thinking, effort)?, options);
        if lower.contains("glm-5.2") {
            if let Some(effort_value) = effort.and_then(ReasoningEffort::deepseek_glm_effort) {
                options.insert("reasoning_effort".into(), serde_json::json!(effort_value));
            }
        } else if effort
            .and_then(ReasoningEffort::deepseek_glm_effort)
            .is_some()
        {
            tracing::warn!(
                model = %profile.model,
                "reasoning_effort is not enabled for this GLM profile; only thinking.type will be sent"
            );
        }
        return Ok(());
    }

    if provider == "kimi"
        || provider == "moonshot"
        || base_url.contains("moonshot")
        || lower.contains("kimi")
    {
        return apply_kimi_reasoning_options(profile, thinking, effort, options);
    }

    if provider == "openai"
        || base_url.contains("api.openai.com")
        || lower.starts_with("gpt-")
        || lower.starts_with('o')
    {
        return apply_openai_reasoning_options(profile, thinking, effort, options);
    }

    if effort.is_some() || thinking.is_some() {
        bail!(
            "reasoning controls are not supported for model profile {} (model: {})",
            profile.id,
            profile.model
        );
    }

    Ok(())
}

fn merged_reasoning_controls(
    thinking: Option<ThinkingMode>,
    effort: Option<ReasoningEffort>,
) -> Result<Option<(Option<ThinkingMode>, Option<ReasoningEffort>)>> {
    let effort = match effort {
        Some(ReasoningEffort::Auto) => None,
        other => other,
    };
    if thinking.is_none() && effort.is_none() {
        return Ok(None);
    }

    Ok(Some((thinking, effort)))
}

fn thinking_for_effort(
    thinking: Option<ThinkingMode>,
    effort: Option<ReasoningEffort>,
) -> Result<Option<ThinkingMode>> {
    let effort_thinking = effort.and_then(ReasoningEffort::thinking_mode);
    match (thinking, effort_thinking) {
        (Some(ThinkingMode::Enabled), Some(ThinkingMode::Disabled)) => {
            bail!("thinking=enabled conflicts with reasoning_effort=none/minimal")
        }
        (Some(ThinkingMode::Disabled), Some(ThinkingMode::Enabled)) => {
            bail!("thinking=disabled conflicts with reasoning_effort requiring thinking")
        }
        _ => {}
    }

    Ok(thinking.or(effort_thinking))
}

fn apply_thinking_type(
    thinking: Option<ThinkingMode>,
    options: &mut serde_json::Map<String, serde_json::Value>,
) {
    if let Some(thinking_type) = thinking.and_then(ThinkingMode::request_type) {
        options.insert(
            "thinking".into(),
            serde_json::json!({ "type": thinking_type }),
        );
    }
}

fn apply_kimi_reasoning_options(
    profile: &ModelProfileConfig,
    thinking: Option<ThinkingMode>,
    effort: Option<ReasoningEffort>,
    options: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let lower = profile.model.trim().to_ascii_lowercase();

    if lower.contains("kimi-k2.7-code") {
        if matches!(
            thinking,
            Some(ThinkingMode::Disabled) | Some(ThinkingMode::Enabled)
        ) || matches!(
            effort,
            Some(
                ReasoningEffort::None
                    | ReasoningEffort::Minimal
                    | ReasoningEffort::Low
                    | ReasoningEffort::Medium
                    | ReasoningEffort::High
                    | ReasoningEffort::XHigh
                    | ReasoningEffort::Max
            )
        ) {
            bail!(
                "model {} always has thinking enabled and does not accept reasoning controls",
                profile.model
            );
        }
        return Ok(());
    }

    if lower.contains("kimi-k2.6") || lower.contains("kimi-k2.5") {
        apply_thinking_type(thinking_for_effort(thinking, effort)?, options);
        if matches!(
            effort,
            Some(
                ReasoningEffort::Low
                    | ReasoningEffort::Medium
                    | ReasoningEffort::High
                    | ReasoningEffort::XHigh
                    | ReasoningEffort::Max
            )
        ) {
            tracing::warn!(
                model = %profile.model,
                "Kimi supports thinking on/off for this profile but not adjustable reasoning effort"
            );
        }
        return Ok(());
    }

    if lower.contains("kimi-k2-thinking") {
        tracing::warn!(
            model = %profile.model,
            "ignoring thinking override because kimi-k2-thinking always reasons"
        );
        return Ok(());
    }

    bail!(
        "reasoning controls are not supported for Kimi model {}",
        profile.model
    )
}

fn apply_openai_reasoning_options(
    profile: &ModelProfileConfig,
    thinking: Option<ThinkingMode>,
    effort: Option<ReasoningEffort>,
    options: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let lower = profile.model.trim().to_ascii_lowercase();
    if thinking.is_some() {
        bail!(
            "thinking is not an OpenAI chat-completions parameter for model {}",
            profile.model
        );
    }

    let Some(effort_value) = effort.and_then(ReasoningEffort::request_value) else {
        return Ok(());
    };

    if lower.starts_with("gpt-5") {
        options.insert(
            "reasoning".into(),
            serde_json::json!({ "effort": effort_value }),
        );
        return Ok(());
    }

    if lower.starts_with("o1") || lower.starts_with("o3") || lower.starts_with("o4") {
        options.insert("reasoning_effort".into(), serde_json::json!(effort_value));
        return Ok(());
    }

    bail!(
        "reasoning_effort is not supported for OpenAI model {}",
        profile.model
    );
}

#[derive(Debug, Clone)]
pub struct ModelProfileRegistry {
    dir: PathBuf,
    profiles: BTreeMap<String, ModelProfileConfig>,
}

pub fn install_embedded_model_profiles(dir: impl AsRef<Path>) -> Result<()> {
    let dir = dir.as_ref();
    let has_yaml = dir.exists()
        && fs::read_dir(dir)
            .with_context(|| format!("failed to read models dir {}", dir.display()))?
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("yaml"));

    if has_yaml {
        return Ok(());
    }

    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create models dir {}", dir.display()))?;
    for (name, contents) in EMBEDDED_MODEL_PROFILES {
        let path = dir.join(name);
        if !path.exists() {
            fs::write(&path, contents)
                .with_context(|| format!("failed to seed model profile {}", path.display()))?;
        }
    }
    Ok(())
}

impl ModelProfileRegistry {
    pub fn load(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        let mut profiles = BTreeMap::new();

        if dir.exists() {
            for entry in fs::read_dir(&dir)
                .with_context(|| format!("failed to read models dir {}", dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("yaml")
                {
                    continue;
                }
                let profile = load_profile_file(&path)?;
                profile.validate()?;
                if profiles.contains_key(&profile.id) {
                    bail!("duplicate model profile id: {}", profile.id);
                }
                profiles.insert(profile.id.clone(), profile);
            }
        }

        Ok(Self { dir, profiles })
    }

    pub fn get(&self, id: &str) -> Option<&ModelProfileConfig> {
        self.profiles.get(id)
    }

    pub fn default_profile(&self) -> Option<&ModelProfileConfig> {
        self.get(DEFAULT_MODEL_PROFILE_ID)
    }

    pub fn list(&self) -> Vec<&ModelProfileConfig> {
        self.profiles.values().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn load_profile_file(path: &Path) -> Result<ModelProfileConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read model profile {}", path.display()))?;
    serde_yaml::from_str(&raw)
        .with_context(|| format!("failed to parse model profile {}", path.display()))
}

pub fn resolve_model_profile_from_env(
    models_dir: impl Into<PathBuf>,
) -> Result<ResolvedModelProfile> {
    let models_dir = models_dir.into();
    let registry = ModelProfileRegistry::load(models_dir)?;
    let thinking_override = thinking_override_from_env()?;
    let reasoning_override = reasoning_effort_override_from_env()?;
    let selected_id = std::env::var("REMI_MODEL_PROFILE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if let Some(id) = selected_id {
        let profile = registry.get(&id).cloned().ok_or_else(|| {
            anyhow!(
                "REMI_MODEL_PROFILE={} was not found in {}",
                id,
                registry.dir().display()
            )
        })?;
        let extra_options = profile.merged_extra_options(thinking_override, reasoning_override)?;
        return Ok(ResolvedModelProfile {
            profile,
            extra_options,
            source: ModelProfileSource::Registry,
        });
    }

    if let Some(profile) = registry.default_profile().cloned() {
        let extra_options = profile.merged_extra_options(thinking_override, reasoning_override)?;
        return Ok(ResolvedModelProfile {
            profile,
            extra_options,
            source: ModelProfileSource::Registry,
        });
    }

    if !registry.is_empty() {
        let known = registry
            .list()
            .into_iter()
            .map(|profile| profile.id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "model profiles found in {} but no `default` profile exists; set REMI_MODEL_PROFILE to one of: {}",
            registry.dir().display(),
            known
        );
    }

    let profile = legacy_profile_from_env()?;
    let extra_options = profile.merged_extra_options(thinking_override, reasoning_override)?;
    Ok(ResolvedModelProfile {
        profile,
        extra_options,
        source: ModelProfileSource::LegacyEnv,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProfileSource {
    Registry,
    LegacyEnv,
}

#[derive(Debug, Clone)]
pub struct ResolvedModelProfile {
    pub profile: ModelProfileConfig,
    pub extra_options: serde_json::Map<String, serde_json::Value>,
    pub source: ModelProfileSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProfileKeyStatus {
    pub env_keys: Vec<String>,
    pub configured: bool,
}

pub fn model_profile_key_status(profile: &ModelProfileConfig) -> ModelProfileKeyStatus {
    let env_keys = api_key_env_keys_for_profile(profile);
    let configured = env_keys.iter().any(|key| {
        std::env::var(key)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    });
    ModelProfileKeyStatus {
        env_keys,
        configured,
    }
}

pub fn api_key_from_env(profile: &ModelProfileConfig) -> Result<String> {
    let keys = api_key_env_keys_for_profile(profile);
    for key in &keys {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }

    bail!("{} must be set", keys.join(" or "))
}

pub async fn validate_model_profile_api_key(profile: &ModelProfileConfig) -> Result<()> {
    let api_key = api_key_from_env(profile)?;
    let endpoint = format!(
        "{}/models",
        model_api_base_url(profile).trim_end_matches('/')
    );
    let response = reqwest::Client::new()
        .get(&endpoint)
        .bearer_auth(api_key)
        .send()
        .await
        .with_context(|| {
            format!(
                "failed to validate model profile `{}` via {endpoint}",
                profile.id
            )
        })?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    bail!(
        "model profile `{}` API key validation failed: HTTP {}{}",
        profile.id,
        status,
        if body.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", body.trim())
        }
    )
}

fn model_api_base_url(profile: &ModelProfileConfig) -> String {
    profile
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("https://api.openai.com/v1")
        .to_string()
}

fn api_key_env_keys_for_profile(profile: &ModelProfileConfig) -> Vec<String> {
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

    let keys: Vec<&str> = if provider == "deepseek"
        || model.contains("deepseek")
        || base_url.contains("deepseek.com")
    {
        vec!["DEEPSEEK_API_KEY"]
    } else if provider == "mimo" || model.contains("mimo") || base_url.contains("xiaomimimo.com") {
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
    } else if provider == "openai"
        || provider.is_empty()
        || base_url.contains("api.openai.com")
        || model.starts_with("gpt-")
        || model.starts_with('o')
    {
        vec!["OPENAI_API_KEY"]
    } else if !provider.is_empty() {
        return vec![format!("{}_API_KEY", env_key_fragment(&provider))];
    } else {
        vec!["OPENAI_API_KEY"]
    };

    keys.into_iter().map(ToOwned::to_owned).collect()
}

fn env_key_fragment(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

fn thinking_override_from_env() -> Result<Option<ThinkingMode>> {
    match std::env::var("REMI_KIMI_THINKING") {
        Ok(raw) => ThinkingMode::parse(&raw)
            .map(Some)
            .ok_or_else(|| anyhow!("REMI_KIMI_THINKING must be one of: auto, enabled, disabled")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(anyhow!("failed to read REMI_KIMI_THINKING: {err}")),
    }
}

fn reasoning_effort_override_from_env() -> Result<Option<ReasoningEffort>> {
    match std::env::var("REMI_REASONING_EFFORT") {
        Ok(raw) => ReasoningEffort::parse(&raw).map(Some).ok_or_else(|| {
            anyhow!(
                "REMI_REASONING_EFFORT must be one of: auto, none, minimal, low, medium, high, xhigh, max"
            )
        }),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(anyhow!("failed to read REMI_REASONING_EFFORT: {err}")),
    }
}

fn legacy_profile_from_env() -> Result<ModelProfileConfig> {
    let model = std::env::var("OPENAI_MODEL")
        .or_else(|_| std::env::var("REMI_MODEL"))
        .unwrap_or_else(|_| "gpt-4o".to_string());
    let base_url = std::env::var("OPENAI_BASE_URL")
        .or_else(|_| std::env::var("REMI_BASE_URL"))
        .ok()
        .filter(|value| !value.trim().is_empty());
    let short_term_tokens = std::env::var("REMI_SHORT_TERM_TOKENS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8_192);
    let overflow_bytes = std::env::var("REMI_OVERFLOW_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16_384);

    let profile = ModelProfileConfig {
        id: DEFAULT_MODEL_PROFILE_ID.to_string(),
        name: format!("Legacy {model}"),
        description: Some("Compatibility profile built from legacy environment variables.".into()),
        provider: None,
        model,
        base_url,
        thinking: None,
        reasoning_effort: None,
        max_output_tokens: LEGACY_FALLBACK_MAX_OUTPUT_TOKENS,
        context_tokens: LEGACY_FALLBACK_CONTEXT_TOKENS,
        supports_images: legacy_supports_images_guess(
            &std::env::var("OPENAI_MODEL")
                .or_else(|_| std::env::var("REMI_MODEL"))
                .unwrap_or_else(|_| "gpt-4o".to_string()),
        ),
        short_term_tokens,
        overflow_bytes,
        auto_compress: true,
        extra_options: serde_json::Map::new(),
    };
    profile.validate()?;
    Ok(profile)
}

fn legacy_supports_images_guess(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let imageish = [
        "gpt-4o", "gpt-4.1", "gemini", "claude-3", "qwen-vl", "vision", "vl-",
    ];
    imageish.iter().any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    struct EnvGuard {
        saved: HashMap<&'static str, Option<String>>,
    }

    impl EnvGuard {
        fn capture(keys: &[&'static str]) -> Self {
            let saved = keys
                .iter()
                .map(|key| (*key, std::env::var(key).ok()))
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn write_profile(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    fn test_profile(
        provider: Option<&str>,
        model: &str,
        base_url: Option<&str>,
    ) -> ModelProfileConfig {
        ModelProfileConfig {
            id: model.to_string(),
            name: model.to_string(),
            description: None,
            provider: provider.map(ToOwned::to_owned),
            model: model.to_string(),
            base_url: base_url.map(ToOwned::to_owned),
            thinking: None,
            reasoning_effort: None,
            max_output_tokens: 4096,
            context_tokens: 128000,
            supports_images: false,
            short_term_tokens: 8192,
            overflow_bytes: 16384,
            auto_compress: true,
            extra_options: serde_json::Map::new(),
        }
    }

    #[test]
    fn loads_multiple_model_profiles() {
        let temp = tempdir().unwrap();
        write_profile(
            temp.path(),
            "default.yaml",
            r#"
id: default
name: Default
model: gpt-4o
base_url: https://api.openai.com/v1
max_output_tokens: 4096
context_tokens: 128000
supports_images: true
short_term_tokens: 8192
overflow_bytes: 16384
auto_compress: true
"#,
        );
        write_profile(
            temp.path(),
            "deepseek.yaml",
            r#"
id: deepseek-v4-flash
name: DeepSeek
model: deepseek-v4-flash
base_url: https://api.deepseek.com
max_output_tokens: 8192
context_tokens: 128000
supports_images: false
short_term_tokens: 12000
overflow_bytes: 24000
auto_compress: false
"#,
        );

        let registry = ModelProfileRegistry::load(temp.path()).unwrap();
        assert_eq!(registry.list().len(), 2);
        assert!(registry.get("default").is_some());
        assert!(registry.get("deepseek-v4-flash").is_some());
    }

    #[test]
    fn duplicate_profile_ids_error() {
        let temp = tempdir().unwrap();
        write_profile(
            temp.path(),
            "a.yaml",
            r#"
id: same
name: One
model: a
max_output_tokens: 1
context_tokens: 1
supports_images: false
short_term_tokens: 1
overflow_bytes: 1
auto_compress: true
"#,
        );
        write_profile(
            temp.path(),
            "b.yaml",
            r#"
id: same
name: Two
model: b
max_output_tokens: 1
context_tokens: 1
supports_images: false
short_term_tokens: 1
overflow_bytes: 1
auto_compress: true
"#,
        );

        let err = ModelProfileRegistry::load(temp.path()).unwrap_err();
        assert!(err.to_string().contains("duplicate model profile id"));
    }

    #[test]
    fn invalid_profile_is_rejected() {
        let temp = tempdir().unwrap();
        write_profile(
            temp.path(),
            "bad.yaml",
            r#"
id: default
name: ""
model: gpt-4o
max_output_tokens: 4096
context_tokens: 128000
supports_images: true
short_term_tokens: 8192
overflow_bytes: 16384
auto_compress: true
"#,
        );
        let err = ModelProfileRegistry::load(temp.path()).unwrap_err();
        assert!(err.to_string().contains("missing name"));
    }

    #[test]
    fn env_selects_named_profile() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&[
            "REMI_MODEL_PROFILE",
            "REMI_KIMI_THINKING",
            "REMI_REASONING_EFFORT",
        ]);
        let temp = tempdir().unwrap();
        write_profile(
            temp.path(),
            "default.yaml",
            r#"
id: default
name: Default
model: gpt-4o
max_output_tokens: 4096
context_tokens: 128000
supports_images: true
short_term_tokens: 8192
overflow_bytes: 16384
auto_compress: true
"#,
        );
        write_profile(
            temp.path(),
            "deepseek.yaml",
            r#"
id: deepseek-v4-flash
name: DeepSeek
model: deepseek-v4-flash
base_url: https://api.deepseek.com
max_output_tokens: 8192
context_tokens: 128000
supports_images: false
short_term_tokens: 12000
overflow_bytes: 24000
auto_compress: false
"#,
        );
        unsafe {
            std::env::set_var("REMI_MODEL_PROFILE", "deepseek-v4-flash");
            std::env::remove_var("REMI_KIMI_THINKING");
            std::env::remove_var("REMI_REASONING_EFFORT");
        }

        let resolved = resolve_model_profile_from_env(temp.path()).unwrap();
        assert_eq!(resolved.profile.id, "deepseek-v4-flash");
        assert_eq!(resolved.source, ModelProfileSource::Registry);
    }

    #[test]
    fn env_defaults_to_default_profile() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&[
            "REMI_MODEL_PROFILE",
            "REMI_KIMI_THINKING",
            "REMI_REASONING_EFFORT",
        ]);
        let temp = tempdir().unwrap();
        write_profile(
            temp.path(),
            "default.yaml",
            r#"
id: default
name: Default
model: gpt-4o
max_output_tokens: 4096
context_tokens: 128000
supports_images: true
short_term_tokens: 8192
overflow_bytes: 16384
auto_compress: true
"#,
        );
        unsafe {
            std::env::remove_var("REMI_MODEL_PROFILE");
            std::env::remove_var("REMI_KIMI_THINKING");
            std::env::remove_var("REMI_REASONING_EFFORT");
        }

        let resolved = resolve_model_profile_from_env(temp.path()).unwrap();
        assert_eq!(resolved.profile.id, "default");
    }

    #[test]
    fn falls_back_to_legacy_env_when_registry_empty() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&[
            "REMI_MODEL_PROFILE",
            "REMI_KIMI_THINKING",
            "REMI_REASONING_EFFORT",
            "OPENAI_MODEL",
            "OPENAI_BASE_URL",
            "REMI_SHORT_TERM_TOKENS",
            "REMI_OVERFLOW_BYTES",
        ]);
        let temp = tempdir().unwrap();
        unsafe {
            std::env::remove_var("REMI_MODEL_PROFILE");
            std::env::remove_var("REMI_KIMI_THINKING");
            std::env::remove_var("REMI_REASONING_EFFORT");
            std::env::set_var("OPENAI_MODEL", "gpt-4o");
            std::env::set_var("OPENAI_BASE_URL", "https://api.openai.com/v1");
            std::env::set_var("REMI_SHORT_TERM_TOKENS", "9999");
            std::env::set_var("REMI_OVERFLOW_BYTES", "22222");
        }

        let resolved = resolve_model_profile_from_env(temp.path()).unwrap();
        assert_eq!(resolved.source, ModelProfileSource::LegacyEnv);
        assert_eq!(resolved.profile.model, "gpt-4o");
        assert_eq!(
            resolved.profile.base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(resolved.profile.short_term_tokens, 9999);
        assert_eq!(resolved.profile.overflow_bytes, 22222);
    }

    #[test]
    fn kimi_thinking_override_merges_into_profile() {
        let profile = ModelProfileConfig {
            id: "default".into(),
            name: "Kimi".into(),
            description: None,
            provider: None,
            model: "kimi-k2.5".into(),
            base_url: None,
            thinking: None,
            reasoning_effort: None,
            max_output_tokens: 4096,
            context_tokens: 128000,
            supports_images: false,
            short_term_tokens: 8192,
            overflow_bytes: 16384,
            auto_compress: true,
            extra_options: serde_json::Map::new(),
        };

        let options = profile
            .merged_extra_options(Some(ThinkingMode::Disabled), None)
            .unwrap();
        assert_eq!(
            options.get("thinking"),
            Some(&serde_json::json!({ "type": "disabled" }))
        );
    }

    #[test]
    fn reasoning_effort_maps_to_deepseek_request_options() {
        let profile = test_profile(
            Some("deepseek"),
            "deepseek-v4-pro",
            Some("https://api.deepseek.com"),
        );

        let options = profile
            .merged_extra_options(None, Some(ReasoningEffort::Max))
            .unwrap();

        assert_eq!(
            options.get("thinking"),
            Some(&serde_json::json!({ "type": "enabled" }))
        );
        assert_eq!(
            options.get("reasoning_effort"),
            Some(&serde_json::json!("max"))
        );
    }

    #[test]
    fn reasoning_effort_maps_to_glm_request_options() {
        let profile = test_profile(
            Some("glm"),
            "glm-5.2",
            Some("https://open.bigmodel.cn/api/paas/v4"),
        );

        let options = profile
            .merged_extra_options(None, Some(ReasoningEffort::Medium))
            .unwrap();

        assert_eq!(
            options.get("thinking"),
            Some(&serde_json::json!({ "type": "enabled" }))
        );
        assert_eq!(
            options.get("reasoning_effort"),
            Some(&serde_json::json!("high"))
        );
    }

    #[test]
    fn reasoning_effort_none_disables_kimi_thinking() {
        let profile = test_profile(
            Some("kimi"),
            "kimi-k2.6",
            Some("https://api.moonshot.cn/v1"),
        );

        let options = profile
            .merged_extra_options(None, Some(ReasoningEffort::None))
            .unwrap();

        assert_eq!(
            options.get("thinking"),
            Some(&serde_json::json!({ "type": "disabled" }))
        );
    }

    #[test]
    fn reasoning_effort_rejects_gpt4o() {
        let profile = test_profile(Some("openai"), "gpt-4o", Some("https://api.openai.com/v1"));

        let err = profile
            .merged_extra_options(None, Some(ReasoningEffort::High))
            .unwrap_err();

        assert!(err.to_string().contains("not supported for OpenAI model"));
    }

    #[test]
    fn installs_embedded_profiles_into_empty_dir() {
        let temp = tempdir().unwrap();
        let dir = temp.path().join("models");
        install_embedded_model_profiles(&dir).unwrap();

        assert!(dir.join("default.yaml").exists());
        assert!(dir.join("gpt-4o.yaml").exists());
        assert!(dir.join("deepseek-v4-flash.yaml").exists());
        assert!(dir.join("deepseek-v4-pro.yaml").exists());
        assert!(dir.join("mimo-v2.5-pro.yaml").exists());
        assert!(dir.join("mimo-v2.5.yaml").exists());
        assert!(dir.join("kimi-k2.7-code.yaml").exists());
        assert!(dir.join("kimi-k2.6.yaml").exists());
        assert!(dir.join("glm-5.2.yaml").exists());
        assert!(dir.join("glm-5v-turbo.yaml").exists());
    }

    #[test]
    fn mimo_profiles_prefer_mimo_api_key() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&["MIMO_API_KEY", "OPENAI_API_KEY"]);
        let profile = ModelProfileConfig {
            id: "mimo-v2.5-pro".into(),
            name: "MiMo".into(),
            description: None,
            provider: Some("mimo".into()),
            model: "mimo-v2.5-pro".into(),
            base_url: Some("https://api.xiaomimimo.com/v1".into()),
            thinking: None,
            reasoning_effort: None,
            max_output_tokens: 131072,
            context_tokens: 1000000,
            supports_images: false,
            short_term_tokens: 24000,
            overflow_bytes: 32000,
            auto_compress: true,
            extra_options: serde_json::Map::new(),
        };

        unsafe {
            std::env::set_var("MIMO_API_KEY", "mimo-key");
            std::env::set_var("OPENAI_API_KEY", "openai-key");
        }

        assert_eq!(api_key_from_env(&profile).unwrap(), "mimo-key");
    }

    #[test]
    fn deepseek_profiles_prefer_deepseek_api_key() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&["DEEPSEEK_API_KEY", "OPENAI_API_KEY"]);
        let profile = ModelProfileConfig {
            id: "deepseek-v4-pro".into(),
            name: "DeepSeek".into(),
            description: None,
            provider: Some("deepseek".into()),
            model: "deepseek-v4-pro".into(),
            base_url: Some("https://api.deepseek.com".into()),
            thinking: None,
            reasoning_effort: None,
            max_output_tokens: 393216,
            context_tokens: 1000000,
            supports_images: false,
            short_term_tokens: 24000,
            overflow_bytes: 32000,
            auto_compress: true,
            extra_options: serde_json::Map::new(),
        };

        unsafe {
            std::env::set_var("DEEPSEEK_API_KEY", "deepseek-key");
            std::env::set_var("OPENAI_API_KEY", "openai-key");
        }

        assert_eq!(api_key_from_env(&profile).unwrap(), "deepseek-key");
    }

    #[test]
    fn kimi_profiles_prefer_moonshot_api_key() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&["MOONSHOT_API_KEY", "KIMI_API_KEY", "OPENAI_API_KEY"]);
        let profile = ModelProfileConfig {
            id: "kimi-k2.7-code".into(),
            name: "Kimi".into(),
            description: None,
            provider: Some("kimi".into()),
            model: "kimi-k2.7-code".into(),
            base_url: Some("https://api.moonshot.cn/v1".into()),
            thinking: None,
            reasoning_effort: None,
            max_output_tokens: 131072,
            context_tokens: 256000,
            supports_images: false,
            short_term_tokens: 24000,
            overflow_bytes: 32000,
            auto_compress: true,
            extra_options: serde_json::Map::new(),
        };

        unsafe {
            std::env::set_var("MOONSHOT_API_KEY", "moonshot-key");
            std::env::set_var("KIMI_API_KEY", "kimi-key");
            std::env::set_var("OPENAI_API_KEY", "openai-key");
        }

        assert_eq!(api_key_from_env(&profile).unwrap(), "moonshot-key");
    }

    #[test]
    fn glm_profiles_prefer_glm_api_key() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&[
            "GLM_API_KEY",
            "ZHIPU_API_KEY",
            "BIGMODEL_API_KEY",
            "OPENAI_API_KEY",
        ]);
        let profile = ModelProfileConfig {
            id: "glm-5.2".into(),
            name: "GLM".into(),
            description: None,
            provider: Some("glm".into()),
            model: "glm-5.2".into(),
            base_url: Some("https://open.bigmodel.cn/api/paas/v4".into()),
            thinking: None,
            reasoning_effort: None,
            max_output_tokens: 131072,
            context_tokens: 1000000,
            supports_images: false,
            short_term_tokens: 24000,
            overflow_bytes: 32000,
            auto_compress: true,
            extra_options: serde_json::Map::new(),
        };

        unsafe {
            std::env::set_var("GLM_API_KEY", "glm-key");
            std::env::set_var("ZHIPU_API_KEY", "zhipu-key");
            std::env::set_var("OPENAI_API_KEY", "openai-key");
            std::env::remove_var("BIGMODEL_API_KEY");
        }

        assert_eq!(api_key_from_env(&profile).unwrap(), "glm-key");
    }

    #[test]
    fn provider_profiles_do_not_fall_back_to_shared_api_keys() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&["DEEPSEEK_API_KEY", "MIMO_API_KEY", "OPENAI_API_KEY"]);
        let deepseek = test_profile(
            Some("deepseek"),
            "deepseek-v4-pro",
            Some("https://api.deepseek.com"),
        );
        let mimo = test_profile(
            Some("mimo"),
            "mimo-v2.5-pro",
            Some("https://api.xiaomimimo.com/v1"),
        );

        unsafe {
            std::env::remove_var("DEEPSEEK_API_KEY");
            std::env::remove_var("MIMO_API_KEY");
            std::env::set_var("OPENAI_API_KEY", "openai-key");
        }

        assert!(api_key_from_env(&deepseek)
            .unwrap_err()
            .to_string()
            .contains("DEEPSEEK_API_KEY must be set"));
        assert!(api_key_from_env(&mimo)
            .unwrap_err()
            .to_string()
            .contains("MIMO_API_KEY must be set"));
    }

    #[test]
    fn openai_profiles_use_openai_api_key_only() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&["OPENAI_API_KEY"]);
        let profile = test_profile(Some("openai"), "gpt-4o", Some("https://api.openai.com/v1"));

        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
        assert!(api_key_from_env(&profile)
            .unwrap_err()
            .to_string()
            .contains("OPENAI_API_KEY must be set"));

        unsafe {
            std::env::set_var("OPENAI_API_KEY", "openai-key");
        }
        assert_eq!(api_key_from_env(&profile).unwrap(), "openai-key");
    }
}
