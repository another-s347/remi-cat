//! Persists the list of registered daemons to
//! `~/.config/remi-admin/daemons.json`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

// ── DaemonEntry ───────────────────────────────────────────────────────────────

/// A registered daemon instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonEntry {
    /// Unique ID (UUID v4).
    pub id: String,
    /// Human-friendly label.
    pub label: String,
    /// Connection address: `"host:port"` (ws:// prefix added automatically).
    pub addr: String,
    /// SHA-256 fingerprint of the daemon's Noise static public key (hex).
    /// `None` until the first successful connection.
    pub fingerprint: Option<String>,
}

// ── Registry ──────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct Registry {
    pub daemons: Vec<DaemonEntry>,
    path: PathBuf,
}

impl Registry {
    /// Load from `~/.config/remi-admin/daemons.json`.
    /// Creates an empty registry if the file does not exist.
    pub fn load(config_dir: &PathBuf) -> Result<Self> {
        let path = config_dir.join("daemons.json");
        let daemons = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {path:?}"))?;
            serde_json::from_str::<Vec<DaemonEntry>>(&raw)
                .with_context(|| format!("parsing {path:?}"))?
        } else {
            vec![]
        };
        info!(count = daemons.len(), "daemon registry loaded");
        Ok(Self { daemons, path })
    }

    /// Persist the current registry to disk (atomic write via temp file).
    fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.daemons)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .with_context(|| format!("writing {tmp:?}"))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming to {:?}", self.path))?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Option<&DaemonEntry> {
        self.daemons.iter().find(|d| d.id == id)
    }

    pub fn add(&mut self, entry: DaemonEntry) -> Result<()> {
        self.daemons.push(entry);
        self.save()
    }

    pub fn remove(&mut self, id: &str) -> Result<bool> {
        let before = self.daemons.len();
        self.daemons.retain(|d| d.id != id);
        let removed = self.daemons.len() < before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// Update the stored fingerprint for a daemon.
    pub fn set_fingerprint(&mut self, id: &str, fingerprint: String) -> Result<()> {
        if let Some(d) = self.daemons.iter_mut().find(|d| d.id == id) {
            d.fingerprint = Some(fingerprint);
            self.save()
        } else {
            Ok(())
        }
    }
}
