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

    fn request_type(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Enabled => Some("enabled"),
            Self::Disabled => Some("disabled"),
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
    ) -> Result<serde_json::Map<String, serde_json::Value>> {
        let mut options = self.extra_options.clone();
        let thinking = thinking_override.or(self.thinking);
        if let Some(mode) = thinking {
            apply_thinking_option(&self.model, mode, &mut options)?;
        }
        Ok(options)
    }
}

fn apply_thinking_option(
    model: &str,
    mode: ThinkingMode,
    options: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let Some(thinking_type) = mode.request_type() else {
        return Ok(());
    };

    let lower = model.trim().to_ascii_lowercase();
    if lower.contains("kimi-k2.5") {
        options.insert(
            "thinking".into(),
            serde_json::json!({ "type": thinking_type }),
        );
        return Ok(());
    }

    if lower.contains("kimi-k2-thinking") {
        tracing::warn!(
            model = %model,
            "ignoring thinking override because kimi-k2-thinking always reasons"
        );
        return Ok(());
    }

    if lower.contains("kimi") {
        tracing::warn!(
            model = %model,
            "ignoring thinking override because only kimi-k2.5 supports toggling thinking"
        );
        return Ok(());
    }

    bail!("thinking override is only supported for kimi-k2.5 profiles right now (model: {model})");
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
        let extra_options = profile.merged_extra_options(thinking_override)?;
        return Ok(ResolvedModelProfile {
            profile,
            extra_options,
            source: ModelProfileSource::Registry,
        });
    }

    if let Some(profile) = registry.default_profile().cloned() {
        let extra_options = profile.merged_extra_options(thinking_override)?;
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
    let extra_options = profile.merged_extra_options(thinking_override)?;
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

fn thinking_override_from_env() -> Result<Option<ThinkingMode>> {
    match std::env::var("REMI_KIMI_THINKING") {
        Ok(raw) => ThinkingMode::parse(&raw)
            .map(Some)
            .ok_or_else(|| anyhow!("REMI_KIMI_THINKING must be one of: auto, enabled, disabled")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(anyhow!("failed to read REMI_KIMI_THINKING: {err}")),
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
        let _guard = EnvGuard::capture(&["REMI_MODEL_PROFILE", "REMI_KIMI_THINKING"]);
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
        }

        let resolved = resolve_model_profile_from_env(temp.path()).unwrap();
        assert_eq!(resolved.profile.id, "deepseek-v4-flash");
        assert_eq!(resolved.source, ModelProfileSource::Registry);
    }

    #[test]
    fn env_defaults_to_default_profile() {
        let _lock = env_lock().lock().unwrap();
        let _guard = EnvGuard::capture(&["REMI_MODEL_PROFILE", "REMI_KIMI_THINKING"]);
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
            "OPENAI_MODEL",
            "OPENAI_BASE_URL",
            "REMI_SHORT_TERM_TOKENS",
            "REMI_OVERFLOW_BYTES",
        ]);
        let temp = tempdir().unwrap();
        unsafe {
            std::env::remove_var("REMI_MODEL_PROFILE");
            std::env::remove_var("REMI_KIMI_THINKING");
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
            max_output_tokens: 4096,
            context_tokens: 128000,
            supports_images: false,
            short_term_tokens: 8192,
            overflow_bytes: 16384,
            auto_compress: true,
            extra_options: serde_json::Map::new(),
        };

        let options = profile
            .merged_extra_options(Some(ThinkingMode::Disabled))
            .unwrap();
        assert_eq!(
            options.get("thinking"),
            Some(&serde_json::json!({ "type": "disabled" }))
        );
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
    }
}
