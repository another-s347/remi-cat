use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::instance_profile::{validate_profile_name, InstanceProfile};

const REGISTRY_SCHEMA_VERSION: u32 = 1;
const REGISTRY_FILE: &str = "profile-registry.json";
const REGISTRY_LOCK: &str = "profile-registry.lock";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisteredProfile {
    pub alias: String,
    pub id: String,
    pub manifest_path: PathBuf,
    pub registered_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryDocument {
    schema_version: u32,
    #[serde(default)]
    profiles: Vec<RegisteredProfile>,
}

impl Default for RegistryDocument {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            profiles: Vec::new(),
        }
    }
}

pub struct ProfileRegistry {
    data_root: PathBuf,
    document: RegistryDocument,
}

impl ProfileRegistry {
    pub fn load(data_root: impl Into<PathBuf>) -> Result<Self> {
        let data_root = data_root.into();
        let document = load_document(&data_root)?;
        Ok(Self {
            data_root,
            document,
        })
    }

    pub fn path(&self) -> PathBuf {
        self.data_root.join(REGISTRY_FILE)
    }

    pub fn entries(&self) -> &[RegisteredProfile] {
        &self.document.profiles
    }

    pub fn resolve(&self, reference: &str) -> Result<InstanceProfile> {
        let reference = reference.trim();
        if reference == "default" {
            return Ok(InstanceProfile::default_in_data_root(&self.data_root));
        }
        if looks_like_path(reference) {
            return InstanceProfile::from_manifest(reference);
        }
        if let Some(alias) = reference.strip_prefix('@') {
            return self.resolve_alias(alias);
        }
        if let Some(id) = reference.strip_prefix("id:") {
            let matches = self
                .document
                .profiles
                .iter()
                .filter(|entry| entry.id == id)
                .collect::<Vec<_>>();
            return match matches.as_slice() {
                [] => anyhow::bail!(
                    "PROFILE_NOT_FOUND: no registered profile has id `{id}`; run `remi-cat profile list`"
                ),
                [entry] => InstanceProfile::from_manifest(&entry.manifest_path),
                _ => anyhow::bail!(
                    "PROFILE_AMBIGUOUS: id `{id}` has multiple aliases; select one with `@alias`"
                ),
            };
        }
        if self
            .document
            .profiles
            .iter()
            .any(|entry| entry.alias == reference)
        {
            eprintln!("Warning: bare profile names are deprecated; use `@{reference}` instead.");
            return self.resolve_alias(reference);
        }
        let legacy_dir = self.data_root.join("profiles").join(reference);
        if legacy_dir.exists() {
            return InstanceProfile::from_label_in_data_root(reference, &self.data_root);
        }
        anyhow::bail!(
            "PROFILE_NOT_FOUND: `{reference}` is not registered and no legacy profile exists; try `@{reference}`, `id:{reference}`, or a profile.yaml path"
        )
    }

    pub fn registration_for_path(&self, path: &Path) -> Option<&RegisteredProfile> {
        let canonical = canonical_or_absolute(path);
        self.document
            .profiles
            .iter()
            .find(|entry| canonical_or_absolute(&entry.manifest_path) == canonical)
    }

    pub fn register(
        &mut self,
        path: &Path,
        alias: Option<&str>,
        replace: bool,
    ) -> Result<RegisteredProfile> {
        let manifest_path = if path.is_dir() {
            path.join(crate::instance_profile::PROFILE_FILE_NAME)
        } else {
            path.to_path_buf()
        };
        let profile = InstanceProfile::from_manifest(&manifest_path)?;
        let manifest_path = canonical_or_absolute(&manifest_path);
        let alias = alias
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_alias(&profile.manifest.id));
        validate_registry_alias(&alias)?;

        std::fs::create_dir_all(&self.data_root)
            .with_context(|| format!("creating {}", self.data_root.display()))?;
        let _lock = RegistryLock::acquire(&self.data_root)?;
        self.document = load_document(&self.data_root)?;

        if let Some(existing) = self
            .document
            .profiles
            .iter()
            .find(|entry| entry.alias == alias)
        {
            if !replace {
                anyhow::bail!(
                    "PROFILE_ALIAS_CONFLICT: `@{alias}` already points to {}; pass --replace to update it",
                    existing.manifest_path.display()
                );
            }
        }
        self.document.profiles.retain(|entry| entry.alias != alias);
        let entry = RegisteredProfile {
            alias,
            id: profile.manifest.id,
            manifest_path,
            registered_at: chrono::Utc::now().to_rfc3339(),
        };
        self.document.profiles.push(entry.clone());
        self.document.profiles.sort_by(|a, b| a.alias.cmp(&b.alias));
        self.save_unlocked()?;
        Ok(entry)
    }

    pub fn unregister(&mut self, reference: &str) -> Result<RegisteredProfile> {
        let alias = reference.strip_prefix('@').unwrap_or(reference);
        std::fs::create_dir_all(&self.data_root)
            .with_context(|| format!("creating {}", self.data_root.display()))?;
        let _lock = RegistryLock::acquire(&self.data_root)?;
        self.document = load_document(&self.data_root)?;
        let index = self
            .document
            .profiles
            .iter()
            .position(|entry| entry.alias == alias)
            .ok_or_else(|| {
                anyhow::anyhow!("PROFILE_NOT_FOUND: alias `@{alias}` is not registered")
            })?;
        let removed = self.document.profiles.remove(index);
        self.save_unlocked()?;
        Ok(removed)
    }

    pub fn repair(&mut self) -> Result<usize> {
        std::fs::create_dir_all(&self.data_root)
            .with_context(|| format!("creating {}", self.data_root.display()))?;
        let _lock = RegistryLock::acquire(&self.data_root)?;
        self.document = load_document(&self.data_root)?;
        let before = self.document.profiles.len();
        self.document
            .profiles
            .retain(|entry| entry.manifest_path.exists());
        let removed = before - self.document.profiles.len();
        if removed > 0 {
            self.save_unlocked()?;
        }
        Ok(removed)
    }

    fn resolve_alias(&self, alias: &str) -> Result<InstanceProfile> {
        let entry = self
            .document
            .profiles
            .iter()
            .find(|entry| entry.alias == alias)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "PROFILE_NOT_FOUND: alias `@{alias}` is not registered; run `remi-cat profile list`"
                )
            })?;
        InstanceProfile::from_manifest(&entry.manifest_path).with_context(|| {
            format!(
                "registered profile `@{alias}` points to {}",
                entry.manifest_path.display()
            )
        })
    }

    fn save_unlocked(&self) -> Result<()> {
        let path = self.path();
        let temporary = self
            .data_root
            .join(format!(".{REGISTRY_FILE}.{}.tmp", std::process::id()));
        let raw = serde_json::to_string_pretty(&self.document)?;
        std::fs::write(&temporary, format!("{raw}\n"))
            .with_context(|| format!("writing {}", temporary.display()))?;
        crate::atomic_file::replace(&temporary, &path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

fn load_document(data_root: &Path) -> Result<RegistryDocument> {
    let path = data_root.join(REGISTRY_FILE);
    if !path.exists() {
        return Ok(RegistryDocument::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading profile registry {}", path.display()))?;
    let parsed: RegistryDocument = serde_json::from_str(&raw)
        .with_context(|| format!("parsing profile registry {}", path.display()))?;
    if parsed.schema_version != REGISTRY_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported profile registry schema {} at {}; expected {}",
            parsed.schema_version,
            path.display(),
            REGISTRY_SCHEMA_VERSION
        );
    }
    Ok(parsed)
}

struct RegistryLock {
    path: PathBuf,
}

impl RegistryLock {
    fn acquire(data_root: &Path) -> Result<Self> {
        let path = data_root.join(REGISTRY_LOCK);
        let started = Instant::now();
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if started.elapsed() >= Duration::from_secs(5) {
                        anyhow::bail!(
                            "PROFILE_REGISTRY_BUSY: timed out waiting for {}",
                            path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(err) => return Err(err).with_context(|| format!("locking {}", path.display())),
            }
        }
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

fn validate_registry_alias(alias: &str) -> Result<()> {
    if alias == "default" {
        anyhow::bail!("`default` is reserved for the builtin profile");
    }
    validate_profile_name(alias)
}

fn default_alias(id: &str) -> String {
    let mut alias = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    while alias.contains("--") {
        alias = alias.replace("--", "-");
    }
    alias.trim_matches('-').to_string()
}

fn looks_like_path(value: &str) -> bool {
    value.ends_with(".yaml")
        || value.ends_with(".yml")
        || value.contains('/')
        || value.contains('\\')
        || Path::new(value).is_dir()
}

fn canonical_or_absolute(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::ProfileRegistry;
    use std::sync::{Arc, Barrier};

    #[test]
    fn register_resolve_and_unregister_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let manifest = project.path().join("profile.yaml");
        std::fs::write(
            &manifest,
            "schema_version: 1\nid: travel.planner\nname: Travel\nendpoint:\n  type: local\n  command: sleep 1\n",
        )
        .unwrap();

        let mut registry = ProfileRegistry::load(root.path()).unwrap();
        registry.register(&manifest, Some("travel"), false).unwrap();
        assert_eq!(
            registry.resolve("@travel").unwrap().manifest.id,
            "travel.planner"
        );

        let removed = registry.unregister("@travel").unwrap();
        assert_eq!(removed.alias, "travel");
        assert!(manifest.exists());
    }

    #[test]
    fn concurrent_registration_preserves_both_entries() {
        let root = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_manifest = first.path().join("profile.yaml");
        let second_manifest = second.path().join("profile.yaml");
        std::fs::write(
            &first_manifest,
            "schema_version: 1\nid: first.agent\nname: First\nendpoint:\n  type: local\n  command: sleep 1\n",
        )
        .unwrap();
        std::fs::write(
            &second_manifest,
            "schema_version: 1\nid: second.agent\nname: Second\nendpoint:\n  type: local\n  command: sleep 1\n",
        )
        .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let threads = [(first_manifest, "first"), (second_manifest, "second")]
            .into_iter()
            .map(|(manifest, alias)| {
                let root = root.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut registry = ProfileRegistry::load(root).unwrap();
                    barrier.wait();
                    registry.register(&manifest, Some(alias), false).unwrap();
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        let registry = ProfileRegistry::load(root.path()).unwrap();
        assert_eq!(registry.entries().len(), 2);
        assert!(registry.resolve("@first").is_ok());
        assert!(registry.resolve("@second").is_ok());
    }

    #[test]
    fn unknown_bare_reference_is_not_synthesized() {
        let root = tempfile::tempdir().unwrap();
        let registry = ProfileRegistry::load(root.path()).unwrap();
        let error = registry.resolve("missing").unwrap_err().to_string();
        assert!(error.contains("PROFILE_NOT_FOUND"));
    }
}
