use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::runtime_config::{detect_setup_state_at, RuntimeConfig, SetupState};

pub const DEFAULT_DATA_DIR: &str = ".remi-cat";
pub const TUI_HOME_DATA_DIR: &str = ".remi_cat";
pub const TUI_HOME_COMPAT_DATA_DIR: &str = ".remi-cat";
pub const DIAGNOSTIC_PROFILE_NAME: &str = "remi_diagnostics";
const PROFILES_DIR: &str = "profiles";
pub const PROFILE_FILE_NAME: &str = "profile.yaml";
pub const PROFILE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfigRefs {
    pub runtime: Option<PathBuf>,
    pub channels: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileResourceRefs {
    pub agents: Option<PathBuf>,
    pub models: Option<PathBuf>,
    pub skills: Vec<PathBuf>,
    pub workflows: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileStateRefs {
    /// Compatibility root for state that has not yet been split into a
    /// dedicated store. Individual references below take precedence.
    pub data: Option<PathBuf>,
    pub sessions: Option<PathBuf>,
    pub memory: Option<PathBuf>,
    pub users: Option<PathBuf>,
    pub tasks: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileCapabilities {
    pub tags: Vec<String>,
    pub intents: Vec<String>,
    pub channels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileEndpointAuth {
    Bearer { token_env: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileEndpoint {
    Local {
        command: String,
    },
    Remote {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth: Option<ProfileEndpointAuth>,
    },
}

/// Serializable resource assembly manifest for a runnable agent application.
///
/// A profile references configuration and resources; it neither owns nor
/// isolates them. Relative paths are resolved from the manifest directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApplicationProfileManifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(default)]
    pub config: ProfileConfigRefs,
    #[serde(default)]
    pub resources: ProfileResourceRefs,
    #[serde(default)]
    pub state: ProfileStateRefs,
    #[serde(default)]
    pub capabilities: ProfileCapabilities,
    pub endpoint: ProfileEndpoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceProfile {
    pub name: Option<String>,
    pub data_dir: PathBuf,
    pub manifest_path: Option<PathBuf>,
    pub manifest: ApplicationProfileManifest,
    pub runtime_config: PathBuf,
    pub channels_config: PathBuf,
    pub workspace: Option<PathBuf>,
    pub agents_dir: PathBuf,
    pub models_dir: PathBuf,
    pub skills_dirs: Vec<PathBuf>,
    pub workflows_dir: PathBuf,
    pub sessions_path: PathBuf,
    pub memory_dir: PathBuf,
    pub users_path: PathBuf,
    pub tasks_dir: PathBuf,
    pub endpoint: ProfileEndpoint,
}

impl InstanceProfile {
    pub fn default_instance() -> Self {
        Self::default_in_data_root(Path::new(DEFAULT_DATA_DIR))
    }

    pub fn default_in_data_root(data_root: &Path) -> Self {
        Self::legacy(None, data_root.to_path_buf())
    }

    pub fn named(name: &str) -> Result<Self> {
        validate_profile_name(name)?;
        Self::load_or_legacy(Some(name.to_string()), profiles_root().join(name))
    }

    pub fn named_in_data_root(name: &str, data_root: &Path) -> Result<Self> {
        validate_profile_name(name)?;
        Self::load_or_legacy(
            Some(name.to_string()),
            data_root.join(PROFILES_DIR).join(name),
        )
    }

    pub fn from_label(label: &str) -> Result<Self> {
        Self::from_label_in_data_root(label, Path::new(DEFAULT_DATA_DIR))
    }

    pub fn from_label_in_data_root(label: &str, data_root: &Path) -> Result<Self> {
        if label == "default" {
            Ok(Self::default_in_data_root(data_root))
        } else {
            Self::named_in_data_root(label, data_root)
        }
    }

    pub fn from_manifest(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let path = if path.is_dir() {
            path.join(PROFILE_FILE_NAME)
        } else {
            path.to_path_buf()
        };
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading application profile {}", path.display()))?;
        let manifest: ApplicationProfileManifest = serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing application profile {}", path.display()))?;
        validate_manifest(&manifest)?;
        Self::resolve_manifest(path, manifest)
    }

    pub fn builtin_default(data_root: impl Into<PathBuf>) -> Self {
        Self::legacy(None, data_root.into())
    }

    pub fn write_manifest(&self) -> Result<PathBuf> {
        let path = self
            .manifest_path
            .clone()
            .unwrap_or_else(|| self.data_dir.join(PROFILE_FILE_NAME));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut manifest = self.manifest.clone();
        if self.manifest_path.is_none() {
            manifest.state.data = Some(PathBuf::from("."));
        }
        let raw = serde_yaml::to_string(&manifest).context("serializing application profile")?;
        std::fs::write(&path, raw)
            .with_context(|| format!("writing application profile {}", path.display()))?;
        Ok(path)
    }

    pub fn apply_resource_env(&self) {
        set_env_path("REMI_DATA_DIR", &self.data_dir);
        set_env_path("REMI_RUNTIME_CONFIG", &self.runtime_config);
        set_env_path("REMI_CHANNELS_CONFIG", &self.channels_config);
        set_env_path("REMI_AGENTS_DIR", &self.agents_dir);
        set_env_path("REMI_MODELS_DIR", &self.models_dir);
        set_env_path("REMI_MEMORY_DIR", &self.memory_dir);
        set_env_path("REMI_SESSIONS_PATH", &self.sessions_path);
        set_env_path("REMI_USERS_PATH", &self.users_path);
        set_env_path("REMI_TASKS_DIR", &self.tasks_dir);
        set_env_path("REMI_WORKFLOWS_DIR", &self.workflows_dir);
        if let Ok(value) = serde_json::to_string(&self.skills_dirs) {
            unsafe { std::env::set_var("REMI_SKILLS_DIRS", value) };
        }
        if let Some(workspace) = &self.workspace {
            set_env_path("REMI_PROFILE_WORKSPACE", workspace);
        }
        unsafe {
            std::env::set_var("REMI_PROFILE_ID", &self.manifest.id);
            if let Some(path) = &self.manifest_path {
                std::env::set_var("REMI_PROFILE_PATH", path);
            } else {
                std::env::remove_var("REMI_PROFILE_PATH");
            }
        }
    }

    pub fn expanded_local_command(&self) -> Result<String> {
        self.expanded_local_command_for("default")
    }

    pub fn expanded_local_command_for(&self, instance: &str) -> Result<String> {
        let ProfileEndpoint::Local { command } = &self.endpoint else {
            anyhow::bail!(
                "REMOTE_AGENT_NOT_IMPLEMENTED: profile `{}` uses a remote A2A endpoint",
                self.manifest.id
            );
        };
        let profile_path = self
            .manifest_path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "default".to_string());
        let profile_dir = self
            .manifest_path
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or(&self.data_dir)
            .display()
            .to_string();
        Ok(command
            .replace("${PROFILE}", &profile_path)
            .replace("${PROFILE_ID}", &self.manifest.id)
            .replace("${PROFILE_DIR}", &profile_dir)
            .replace("${INSTANCE}", instance)
            .replace(
                "${WORKSPACE}",
                &self
                    .workspace
                    .as_deref()
                    .unwrap_or(&self.data_dir)
                    .display()
                    .to_string(),
            ))
    }

    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or("default")
    }

    pub fn is_named(&self) -> bool {
        self.name.is_some()
    }

    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn log_file(&self) -> PathBuf {
        self.log_dir().join("remi-cat.log")
    }

    fn load_or_legacy(name: Option<String>, data_dir: PathBuf) -> Result<Self> {
        let manifest_path = data_dir.join(PROFILE_FILE_NAME);
        if manifest_path.exists() {
            let mut profile = Self::from_manifest(manifest_path)?;
            profile.name = name;
            return Ok(profile);
        }
        Ok(Self::legacy(name, data_dir))
    }

    fn legacy(name: Option<String>, data_dir: PathBuf) -> Self {
        let label = name.as_deref().unwrap_or("default");
        let endpoint = ProfileEndpoint::Local {
            command: builtin_endpoint_command(name.as_deref()),
        };
        let manifest = ApplicationProfileManifest {
            schema_version: PROFILE_SCHEMA_VERSION,
            id: if label == "default" {
                "remi.default".to_string()
            } else {
                format!("remi.{label}")
            },
            name: if label == "default" {
                "Remi Cat".to_string()
            } else {
                label.to_string()
            },
            description: Some("Built-in remi-cat compatible profile".to_string()),
            version: None,
            workspace: None,
            config: ProfileConfigRefs::default(),
            resources: ProfileResourceRefs::default(),
            state: ProfileStateRefs {
                data: Some(data_dir.clone()),
                ..ProfileStateRefs::default()
            },
            capabilities: ProfileCapabilities {
                tags: vec!["general".to_string()],
                intents: Vec::new(),
                channels: vec![
                    "tui".to_string(),
                    "web".to_string(),
                    "feishu".to_string(),
                    "acp".to_string(),
                ],
            },
            endpoint: endpoint.clone(),
        };
        Self::from_parts(name, data_dir, None, manifest, endpoint)
    }

    fn resolve_manifest(path: PathBuf, manifest: ApplicationProfileManifest) -> Result<Self> {
        let base = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let data_dir = resolve_optional_path(&base, manifest.state.data.as_deref())
            .unwrap_or_else(|| base.join(".remi-cat"));
        let label = Some(manifest.id.clone());
        let endpoint = manifest.endpoint.clone();
        Ok(Self::from_parts(
            label,
            data_dir,
            Some(path),
            manifest,
            endpoint,
        ))
    }

    fn from_parts(
        name: Option<String>,
        data_dir: PathBuf,
        manifest_path: Option<PathBuf>,
        manifest: ApplicationProfileManifest,
        endpoint: ProfileEndpoint,
    ) -> Self {
        let base = manifest_path
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or(&data_dir);
        let resolve = |value: Option<&Path>, fallback: PathBuf| {
            resolve_optional_path(base, value).unwrap_or(fallback)
        };
        let runtime_config = resolve(
            manifest.config.runtime.as_deref(),
            data_dir.join("runtime.yaml"),
        );
        let channels_config = resolve(
            manifest.config.channels.as_deref(),
            data_dir.join("channels.yaml"),
        );
        let agents_dir = resolve(
            manifest.resources.agents.as_deref(),
            data_dir.join("agents"),
        );
        let models_dir = resolve(
            manifest.resources.models.as_deref(),
            data_dir.join("models"),
        );
        let skills_dirs = if manifest.resources.skills.is_empty() {
            vec![data_dir.join("skills")]
        } else {
            manifest
                .resources
                .skills
                .iter()
                .map(|path| resolve_path(base, path))
                .collect()
        };
        let workflows_dir = resolve(
            manifest.resources.workflows.as_deref(),
            data_dir.join("workflows"),
        );
        let sessions_path = resolve(
            manifest.state.sessions.as_deref(),
            data_dir.join("sessions.json"),
        );
        let memory_dir = resolve(manifest.state.memory.as_deref(), data_dir.join("memory"));
        let users_path = resolve(manifest.state.users.as_deref(), data_dir.join("users.json"));
        let tasks_dir = resolve(manifest.state.tasks.as_deref(), data_dir.join("tool_tasks"));
        let workspace = manifest
            .workspace
            .as_deref()
            .map(|path| resolve_path(base, path));
        Self {
            name,
            data_dir,
            manifest_path,
            manifest,
            runtime_config,
            channels_config,
            workspace,
            agents_dir,
            models_dir,
            skills_dirs,
            workflows_dir,
            sessions_path,
            memory_dir,
            users_path,
            tasks_dir,
            endpoint,
        }
    }
}

pub(crate) fn validate_manifest(manifest: &ApplicationProfileManifest) -> Result<()> {
    if manifest.schema_version != PROFILE_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported profile schema_version {}; expected {}",
            manifest.schema_version,
            PROFILE_SCHEMA_VERSION
        );
    }
    validate_profile_id(&manifest.id)?;
    if manifest.name.trim().is_empty() {
        anyhow::bail!("profile name must not be empty");
    }
    match &manifest.endpoint {
        ProfileEndpoint::Local { command } if command.trim().is_empty() => {
            anyhow::bail!("local profile endpoint command must not be empty")
        }
        ProfileEndpoint::Remote { url, .. }
            if !(url.starts_with("http://") || url.starts_with("https://")) =>
        {
            anyhow::bail!("remote profile endpoint URL must use http or https")
        }
        ProfileEndpoint::Remote {
            auth: Some(ProfileEndpointAuth::Bearer { token_env }),
            ..
        } if token_env.trim().is_empty() => {
            anyhow::bail!("remote bearer token_env must not be empty")
        }
        _ => {}
    }
    Ok(())
}

fn validate_profile_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        anyhow::bail!("invalid profile id `{id}`; use ASCII letters, digits, `.`, `-`, and `_`");
    }
    Ok(())
}

fn resolve_optional_path(base: &Path, value: Option<&Path>) -> Option<PathBuf> {
    value.map(|path| resolve_path(base, path))
}

fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    let expanded = expand_home(path);
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

fn expand_home(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return home_dir_from_env().unwrap_or_else(|| path.to_path_buf());
    }
    let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) else {
        return path.to_path_buf();
    };
    home_dir_from_env()
        .map(|home| home.join(rest))
        .unwrap_or_else(|| path.to_path_buf())
}

fn set_env_path(key: &str, path: &Path) {
    unsafe { std::env::set_var(key, path) };
}

fn builtin_endpoint_command(name: Option<&str>) -> String {
    let executable = std::env::current_exe()
        .ok()
        .map(|path| shell_quote(&path.display().to_string()))
        .unwrap_or_else(|| "remi-cat".to_string());
    match name {
        Some(_) => format!("{executable} --profile \"${{PROFILE}}\" a2a stdio"),
        None => format!("{executable} a2a stdio"),
    }
}

#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'\\' | b'.' | b'-' | b'_' | b':')
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'\\' | b'.' | b'-' | b'_' | b':')
    }) {
        return value.to_string();
    }
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub fn profiles_root() -> PathBuf {
    profiles_root_in_data_root(Path::new(DEFAULT_DATA_DIR))
}

pub fn profiles_root_in_data_root(data_root: &Path) -> PathBuf {
    data_root.join(PROFILES_DIR)
}

pub fn tui_home_data_dir() -> PathBuf {
    let Some(home) = home_dir_from_env() else {
        return PathBuf::from(TUI_HOME_DATA_DIR);
    };
    let preferred = home.join(TUI_HOME_DATA_DIR);
    if preferred.exists() {
        return preferred;
    }
    let compat = home.join(TUI_HOME_COMPAT_DATA_DIR);
    if compat.exists() {
        compat
    } else {
        preferred
    }
}

/// Returns the process-global profile registry root.
///
/// Unlike the TUI state directory, this path is deliberately stable and does
/// not participate in the legacy `.remi_cat` compatibility lookup.
pub fn profile_registry_home_dir() -> PathBuf {
    home_dir_from_env()
        .map(|home| home.join(DEFAULT_DATA_DIR))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR))
}

fn home_dir_from_env() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(home) = windows_home_dir_from_env() {
            return Some(home);
        }
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(home) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let drive = std::env::var_os("HOMEDRIVE")?;
    let path = std::env::var_os("HOMEPATH")?;
    if drive.is_empty() || path.is_empty() {
        return None;
    }
    Some(PathBuf::from(format!(
        "{}{}",
        drive.to_string_lossy(),
        path.to_string_lossy()
    )))
}

#[cfg(windows)]
fn windows_home_dir_from_env() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let drive = std::env::var_os("HOMEDRIVE")?;
    let path = std::env::var_os("HOMEPATH")?;
    if drive.is_empty() || path.is_empty() {
        return None;
    }
    Some(PathBuf::from(format!(
        "{}{}",
        drive.to_string_lossy(),
        path.to_string_lossy()
    )))
}

pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "default"
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!(
            "invalid profile name `{name}`; use only ASCII letters, digits, `-`, and `_` (`default` is reserved)"
        );
    }
    Ok(())
}

pub fn discover_profiles_in_data_root(data_root: &Path) -> Result<Vec<InstanceProfile>> {
    let mut profiles = vec![InstanceProfile::default_in_data_root(data_root)];
    let root = profiles_root_in_data_root(data_root);
    if root.exists() {
        for entry in std::fs::read_dir(&root)
            .with_context(|| format!("reading profile directory {}", root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if validate_profile_name(&name).is_ok() {
                profiles.push(InstanceProfile::named_in_data_root(&name, data_root)?);
            }
        }
    }
    profiles[1..].sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

pub fn configured_profiles_excluding_in_data_root(
    data_dir: &Path,
    data_root: &Path,
) -> Result<Vec<RuntimeConfig>> {
    let mut configs = Vec::new();
    for profile in discover_profiles_in_data_root(data_root)? {
        if same_path(&profile.data_dir, data_dir) {
            continue;
        }
        if let SetupState::Initialized { config, .. } =
            detect_setup_state_at(&profile.runtime_config, &profile.data_dir)
        {
            configs.push(config);
        }
    }
    Ok(configs)
}

pub fn remove_named_profile_in_data_root(name: &str, data_root: &Path) -> Result<PathBuf> {
    validate_profile_name(name)?;
    // Remove the registered profile directory, never a state directory that
    // the manifest may reference outside it.
    let profile_dir = profiles_root_in_data_root(data_root).join(name);
    if !profile_dir.exists() {
        anyhow::bail!("profile `{name}` does not exist");
    }
    std::fs::remove_dir_all(&profile_dir)
        .with_context(|| format!("removing profile `{name}` at {}", profile_dir.display()))?;
    Ok(profile_dir)
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::shell_quote;
    use super::{
        profile_registry_home_dir, remove_named_profile_in_data_root, tui_home_data_dir,
        validate_manifest, validate_profile_name, ApplicationProfileManifest, InstanceProfile,
        ProfileEndpoint, DEFAULT_DATA_DIR, TUI_HOME_DATA_DIR,
    };
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn validates_profile_names() {
        for valid in ["dev", "prod-2", "local_test", "A1"] {
            validate_profile_name(valid).unwrap();
        }
        for invalid in ["", ".", "..", "default", "a/b", "a b", "测试"] {
            assert!(
                validate_profile_name(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn resolves_profiles_inside_selected_data_root() {
        let root = std::path::Path::new("/tmp/remi-home");
        let default = InstanceProfile::from_label_in_data_root("default", root).unwrap();
        assert_eq!(default.name, None);
        assert_eq!(default.data_dir, root);

        let named = InstanceProfile::from_label_in_data_root("work", root).unwrap();
        assert_eq!(named.name.as_deref(), Some("work"));
        assert_eq!(named.data_dir, root.join("profiles").join("work"));
    }

    #[test]
    fn tui_home_data_dir_uses_userprofile_when_home_is_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_home = std::env::var_os("HOME");
        let old_userprofile = std::env::var_os("USERPROFILE");
        let old_homedrive = std::env::var_os("HOMEDRIVE");
        let old_homepath = std::env::var_os("HOMEPATH");
        let temp_home =
            std::env::temp_dir().join(format!("remi-userprofile-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::remove_var("HOME");
            std::env::set_var("USERPROFILE", &temp_home);
            std::env::remove_var("HOMEDRIVE");
            std::env::remove_var("HOMEPATH");
        }

        assert_eq!(tui_home_data_dir(), temp_home.join(TUI_HOME_DATA_DIR));

        unsafe {
            restore_env("HOME", old_home);
            restore_env("USERPROFILE", old_userprofile);
            restore_env("HOMEDRIVE", old_homedrive);
            restore_env("HOMEPATH", old_homepath);
        }
    }

    #[test]
    fn profile_registry_home_uses_the_stable_hyphenated_directory() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old_home = std::env::var_os("HOME");
        let old_userprofile = std::env::var_os("USERPROFILE");
        let old_homedrive = std::env::var_os("HOMEDRIVE");
        let old_homepath = std::env::var_os("HOMEPATH");
        let temp_home =
            std::env::temp_dir().join(format!("remi-registry-home-{}", uuid::Uuid::new_v4()));
        unsafe {
            std::env::remove_var("HOME");
            std::env::set_var("USERPROFILE", &temp_home);
            std::env::remove_var("HOMEDRIVE");
            std::env::remove_var("HOMEPATH");
        }

        assert_eq!(
            profile_registry_home_dir(),
            temp_home.join(DEFAULT_DATA_DIR)
        );

        unsafe {
            restore_env("HOME", old_home);
            restore_env("USERPROFILE", old_userprofile);
            restore_env("HOMEDRIVE", old_homedrive);
            restore_env("HOMEPATH", old_homepath);
        }
    }

    #[test]
    fn manifest_resolves_resource_and_state_references() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.yaml");
        std::fs::write(
            &path,
            r#"schema_version: 1
id: travel.planner
name: Travel Planner
workspace: project
config:
  runtime: config/runtime.yaml
  channels: config/channels.yaml
resources:
  agents: definitions/agents
  models: definitions/models
  skills:
    - skills
    - ../shared-skills
  workflows: definitions/workflows
state:
  data: state
  sessions: state/chat-sessions.json
  memory: ../shared-memory
  users: state/users.json
  tasks: state/tasks
capabilities:
  tags: [travel]
  channels: [tui, profile]
endpoint:
  type: local
  command: "travel-agent --profile ${PROFILE}"
"#,
        )
        .unwrap();

        let profile = InstanceProfile::from_manifest(&path).unwrap();
        assert_eq!(profile.manifest.id, "travel.planner");
        assert_eq!(profile.data_dir, dir.path().join("state"));
        assert_eq!(profile.workspace, Some(dir.path().join("project")));
        assert_eq!(
            profile.runtime_config,
            dir.path().join("config/runtime.yaml")
        );
        assert_eq!(
            profile.channels_config,
            dir.path().join("config/channels.yaml")
        );
        assert_eq!(profile.agents_dir, dir.path().join("definitions/agents"));
        assert_eq!(profile.memory_dir, dir.path().join("../shared-memory"));
        assert_eq!(
            profile.sessions_path,
            dir.path().join("state/chat-sessions.json")
        );
        assert!(profile
            .expanded_local_command()
            .unwrap()
            .contains(&path.display().to_string()));
    }

    #[test]
    fn builtin_default_preserves_legacy_layout() {
        let root = std::path::PathBuf::from(".remi-cat");
        let profile = InstanceProfile::builtin_default(&root);
        assert_eq!(profile.manifest.id, "remi.default");
        assert!(profile.manifest_path.is_none());
        assert_eq!(profile.data_dir, root);
        assert_eq!(profile.agents_dir, root.join("agents"));
        assert_eq!(profile.models_dir, root.join("models"));
        assert_eq!(profile.skills_dirs, vec![root.join("skills")]);
        assert_eq!(profile.sessions_path, root.join("sessions.json"));
        assert_eq!(profile.memory_dir, root.join("memory"));
        assert_eq!(profile.users_path, root.join("users.json"));
        assert_eq!(profile.tasks_dir, root.join("tool_tasks"));
    }

    #[test]
    fn persisted_named_profile_endpoint_selects_its_manifest_path() {
        let root = tempfile::tempdir().unwrap();
        let profile = InstanceProfile::named_in_data_root("travel", root.path()).unwrap();
        let manifest_path = profile.write_manifest().unwrap();
        let reloaded = InstanceProfile::from_manifest(&manifest_path).unwrap();
        let command = reloaded.expanded_local_command().unwrap();

        assert!(command.contains(&manifest_path.display().to_string()));
        assert!(!command.contains("--profile travel "));
    }

    #[cfg(windows)]
    #[test]
    fn windows_shell_quote_uses_cmd_compatible_double_quotes() {
        assert_eq!(
            shell_quote(r"C:\Program Files\Remi\remi-cat.exe"),
            r#""C:\Program Files\Remi\remi-cat.exe""#
        );
    }

    #[test]
    fn remote_endpoint_is_reserved_but_validated() {
        let valid: ApplicationProfileManifest = serde_yaml::from_str(
            "schema_version: 1\nid: remote.travel\nname: Remote\nendpoint:\n  type: remote\n  url: https://example.com/a2a\n  auth:\n    type: bearer\n    token_env: TRAVEL_TOKEN\n",
        )
        .unwrap();
        validate_manifest(&valid).unwrap();

        let mut invalid = valid;
        invalid.endpoint = ProfileEndpoint::Remote {
            url: "file:///tmp/agent".to_string(),
            auth: None,
        };
        assert!(validate_manifest(&invalid).is_err());
    }

    #[test]
    fn removing_named_profile_does_not_remove_referenced_state() {
        let root = tempfile::tempdir().unwrap();
        let external_state = tempfile::tempdir().unwrap();
        let profile_dir = root.path().join("profiles/travel");
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(
            profile_dir.join("profile.yaml"),
            format!(
                "schema_version: 1\nid: travel\nname: Travel\nstate:\n  data: {}\nendpoint:\n  type: local\n  command: remi-cat\n",
                external_state.path().display()
            ),
        )
        .unwrap();
        std::fs::write(external_state.path().join("keep"), "state").unwrap();

        let removed = remove_named_profile_in_data_root("travel", root.path()).unwrap();

        assert_eq!(removed, profile_dir);
        assert!(!removed.exists());
        assert_eq!(
            std::fs::read_to_string(external_state.path().join("keep")).unwrap(),
            "state"
        );
    }

    unsafe fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => unsafe { std::env::set_var(key, value) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
