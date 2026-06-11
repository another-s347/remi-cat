use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::runtime_config::{detect_setup_state, RuntimeConfig, SetupState};

pub const DEFAULT_DATA_DIR: &str = ".remi-cat";
const PROFILES_DIR: &str = "profiles";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceProfile {
    pub name: Option<String>,
    pub data_dir: PathBuf,
}

impl InstanceProfile {
    pub fn default_instance() -> Self {
        Self {
            name: None,
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
        }
    }

    pub fn named(name: &str) -> Result<Self> {
        validate_profile_name(name)?;
        Ok(Self {
            name: Some(name.to_string()),
            data_dir: profiles_root().join(name),
        })
    }

    pub fn from_label(label: &str) -> Result<Self> {
        if label == "default" {
            Ok(Self::default_instance())
        } else {
            Self::named(label)
        }
    }

    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or("default")
    }

    pub fn is_named(&self) -> bool {
        self.name.is_some()
    }

    pub fn run_dir(&self) -> PathBuf {
        self.data_dir.join("run")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.run_dir().join("remi-cat.pid.json")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn log_file(&self) -> PathBuf {
        self.log_dir().join("remi-cat.log")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileRunMetadata {
    pub pid: u32,
    pub profile: String,
    pub data_dir: String,
    pub started_at: String,
    pub command: Vec<String>,
    pub log_path: String,
}

pub fn profiles_root() -> PathBuf {
    PathBuf::from(DEFAULT_DATA_DIR).join(PROFILES_DIR)
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

pub fn discover_profiles() -> Result<Vec<InstanceProfile>> {
    let mut profiles = vec![InstanceProfile::default_instance()];
    let root = profiles_root();
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
                profiles.push(InstanceProfile::named(&name)?);
            }
        }
    }
    profiles[1..].sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

pub fn configured_profiles_excluding(data_dir: &Path) -> Result<Vec<RuntimeConfig>> {
    let mut configs = Vec::new();
    for profile in discover_profiles()? {
        if same_path(&profile.data_dir, data_dir) {
            continue;
        }
        if let SetupState::Initialized { config, .. } = detect_setup_state(&profile.data_dir) {
            configs.push(config);
        }
    }
    Ok(configs)
}

pub fn remove_named_profile(name: &str) -> Result<PathBuf> {
    let profile = InstanceProfile::named(name)?;
    if !profile.data_dir.exists() {
        anyhow::bail!("profile `{name}` does not exist");
    }
    std::fs::remove_dir_all(&profile.data_dir).with_context(|| {
        format!(
            "removing profile `{name}` at {}",
            profile.data_dir.display()
        )
    })?;
    Ok(profile.data_dir)
}

pub fn read_run_metadata(profile: &InstanceProfile) -> Result<Option<ProfileRunMetadata>> {
    let path = profile.pid_file();
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let metadata =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(metadata))
}

pub fn write_run_metadata(profile: &InstanceProfile, metadata: &ProfileRunMetadata) -> Result<()> {
    std::fs::create_dir_all(profile.run_dir())
        .with_context(|| format!("creating {}", profile.run_dir().display()))?;
    let raw = serde_json::to_string_pretty(metadata).context("serializing profile run metadata")?;
    std::fs::write(profile.pid_file(), raw)
        .with_context(|| format!("writing {}", profile.pid_file().display()))
}

pub fn remove_run_metadata(profile: &InstanceProfile) -> Result<()> {
    let path = profile.pid_file();
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        read_run_metadata, remove_run_metadata, validate_profile_name, write_run_metadata,
        InstanceProfile, ProfileRunMetadata,
    };

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
    fn run_metadata_round_trip() {
        let dir = std::env::temp_dir().join(format!("remi-profile-meta-{}", uuid::Uuid::new_v4()));
        let profile = InstanceProfile {
            name: Some("meta".to_string()),
            data_dir: dir.clone(),
        };
        let metadata = ProfileRunMetadata {
            pid: 12345,
            profile: "meta".to_string(),
            data_dir: dir.display().to_string(),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            command: vec![
                "remi-cat".to_string(),
                "--profile".to_string(),
                "meta".to_string(),
            ],
            log_path: profile.log_file().display().to_string(),
        };
        write_run_metadata(&profile, &metadata).unwrap();
        assert_eq!(read_run_metadata(&profile).unwrap(), Some(metadata));
        remove_run_metadata(&profile).unwrap();
        assert!(read_run_metadata(&profile).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
