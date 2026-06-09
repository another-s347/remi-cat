use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpChannelBinding {
    pub session_id: String,
    pub platform: String,
    pub channel_id: String,
    pub updated_at: String,
}

#[derive(Debug)]
pub struct AcpBindingStore {
    path: PathBuf,
    bindings: HashMap<String, AcpChannelBinding>,
}

impl AcpBindingStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        let bindings = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).context("failed to parse ACP bindings")?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => return Err(err).context("failed to read ACP bindings"),
        };

        Ok(Self { path, bindings })
    }

    pub fn default_path() -> PathBuf {
        let base = crate::secret_store::SecretStore::resolve_path();
        base.parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("acp_bindings.json")
    }

    fn key(platform: &str, channel_id: &str) -> String {
        format!("{}:{}", platform.trim(), channel_id.trim())
    }

    pub fn get(&self, platform: &str, channel_id: &str) -> Option<AcpChannelBinding> {
        self.bindings.get(&Self::key(platform, channel_id)).cloned()
    }

    pub fn upsert(&mut self, binding: AcpChannelBinding) -> Result<()> {
        self.bindings
            .insert(Self::key(&binding.platform, &binding.channel_id), binding);
        self.save()
    }

    pub fn delete(&mut self, platform: &str, channel_id: &str) -> Result<bool> {
        let removed = self
            .bindings
            .remove(&Self::key(platform, channel_id))
            .is_some();
        self.save()?;
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(&self.bindings)
            .context("failed to serialize ACP bindings")?;
        std::fs::write(&self.path, json)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }
}
