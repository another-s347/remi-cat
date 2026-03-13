//! Persistent store for user-configured Docker bind mounts.
//!
//! Mounts are stored as a JSON array at a path configured via the
//! `VOLUME_STORE_PATH` environment variable, falling back to
//! `<daemon-exe-dir>/volume_mounts.json`.
//!
//! # Atomicity
//!
//! Writes go to `<path>.tmp` and are then renamed over the target file so an
//! interrupted write never corrupts the store.

use std::path::PathBuf;

use anyhow::{Context, Result};
use mgmt_api::VolumeMount;
use tracing::{debug, info};

// ── VolumeStore ───────────────────────────────────────────────────────────────

/// In-memory list of bind mounts backed by a JSON file on disk.
pub struct VolumeStore {
    mounts: Vec<VolumeMount>,
    path: PathBuf,
}

impl VolumeStore {
    /// Resolve the store path:
    /// 1. `VOLUME_STORE_PATH` env var (if set and non-empty).
    /// 2. `<current-exe-dir>/volume_mounts.json`.
    pub fn resolve_path() -> PathBuf {
        if let Ok(p) = std::env::var("VOLUME_STORE_PATH") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        let dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join("volume_mounts.json")
    }

    /// Load from disk.  If the file does not exist an empty store is returned.
    pub fn load(path: PathBuf) -> Result<Self> {
        let mounts = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading volume store {path:?}"))?;
            serde_json::from_str::<Vec<VolumeMount>>(&raw)
                .with_context(|| format!("parsing volume store {path:?}"))?
        } else {
            info!("volume store not found at {path:?} — starting empty");
            Vec::new()
        };
        debug!(path = ?path, count = mounts.len(), "volume store loaded");
        Ok(Self { mounts, path })
    }

    // ── Mutations ─────────────────────────────────────────────────────────

    /// Add or update a mount (identified by `container_path`).
    pub fn add(&mut self, mount: VolumeMount) -> Result<()> {
        if mount.host_path.is_empty() {
            anyhow::bail!("host_path must not be empty");
        }
        if mount.container_path.is_empty() {
            anyhow::bail!("container_path must not be empty");
        }
        if !mount.host_path.starts_with('/') {
            anyhow::bail!("host_path must be an absolute path");
        }
        if !mount.container_path.starts_with('/') {
            anyhow::bail!("container_path must be an absolute path");
        }
        // Replace existing entry with the same container_path, or append.
        if let Some(existing) = self
            .mounts
            .iter_mut()
            .find(|m| m.container_path == mount.container_path)
        {
            *existing = mount;
        } else {
            self.mounts.push(mount);
        }
        self.save()
    }

    /// Remove the mount with the given `container_path`.  No-ops silently if
    /// no such mount exists.
    pub fn remove(&mut self, container_path: &str) -> Result<()> {
        self.mounts.retain(|m| m.container_path != container_path);
        self.save()
    }

    // ── Readers ───────────────────────────────────────────────────────────

    /// All configured mounts.
    pub fn mounts(&self) -> &[VolumeMount] {
        &self.mounts
    }

    // ── Persistence ───────────────────────────────────────────────────────

    fn save(&self) -> Result<()> {
        let json =
            serde_json::to_string_pretty(&self.mounts).context("serialising volume store")?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .with_context(|| format!("writing volume store tmp {tmp:?}"))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming volume store {tmp:?} → {:?}", self.path))?;
        debug!(path = ?self.path, count = self.mounts.len(), "volume store saved");
        Ok(())
    }
}
