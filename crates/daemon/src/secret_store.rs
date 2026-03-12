//! Persistent KV secret store — lives entirely on the Daemon host, never
//! mounted into the Agent container.
//!
//! Secrets are stored as a flat JSON object (`{"KEY": "value", …}`) at a
//! path configured via the `SECRET_STORE_PATH` environment variable, falling
//! back to `<daemon-exe-dir>/secrets.json`.
//!
//! # Atomicity
//!
//! Writes go to `<path>.tmp` and are then renamed over the target file so an
//! interrupted write never corrupts the store.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use tracing::{debug, info};

// ── SecretStore ───────────────────────────────────────────────────────────────

/// In-memory KV store backed by a JSON file on disk.
pub struct SecretStore {
    entries: HashMap<String, String>,
    path: PathBuf,
}

impl SecretStore {
    /// Resolve the store path:
    /// 1. `SECRET_STORE_PATH` env var (if set and non-empty).
    /// 2. `<current-exe-dir>/secrets.json`.
    pub fn resolve_path() -> PathBuf {
        if let Ok(p) = std::env::var("SECRET_STORE_PATH") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        let dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join("secrets.json")
    }

    /// Load from disk.  If the file does not exist an empty store is returned.
    pub fn load(path: PathBuf) -> Result<Self> {
        let entries = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading secret store {path:?}"))?;
            serde_json::from_str::<HashMap<String, String>>(&raw)
                .with_context(|| format!("parsing secret store {path:?}"))?
        } else {
            info!("secret store not found at {path:?} — starting empty");
            HashMap::new()
        };
        debug!(path = ?path, count = entries.len(), "secret store loaded");
        Ok(Self { entries, path })
    }

    // ── Mutations ─────────────────────────────────────────────────────────

    /// Insert or update a key.  Empty values are rejected.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> Result<()> {
        let key = key.into();
        let value = value.into();
        if value.is_empty() {
            anyhow::bail!("secret value must not be empty");
        }
        if key.is_empty() {
            anyhow::bail!("secret key must not be empty");
        }
        // Validate key is a legal env-var name: [A-Z_][A-Z0-9_]*
        // We accept both lower and upper case for convenience.
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || key.starts_with(|c: char| c.is_ascii_digit())
        {
            anyhow::bail!("secret key `{key}` is not a valid environment variable name");
        }
        self.entries.insert(key, value);
        self.save()
    }

    /// Remove a key.  No-ops silently if the key does not exist.
    pub fn delete(&mut self, key: &str) -> Result<()> {
        self.entries.remove(key);
        self.save()
    }

    // ── Readers ───────────────────────────────────────────────────────────

    /// Sorted list of all keys.
    pub fn keys(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        v.sort();
        v
    }

    /// All entries — used for SecretsSync gRPC message.
    pub fn all_entries(&self) -> HashMap<String, String> {
        self.entries.clone()
    }

    // ── Persistence ───────────────────────────────────────────────────────

    fn save(&self) -> Result<()> {
        let json =
            serde_json::to_string_pretty(&self.entries).context("serialising secret store")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .with_context(|| format!("writing secret store tmp {tmp:?}"))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming secret store {tmp:?} → {:?}", self.path))?;
        debug!(path = ?self.path, count = self.entries.len(), "secret store saved");
        Ok(())
    }
}
