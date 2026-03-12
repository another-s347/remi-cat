//! `matcher` — Owner pairing logic.
//!
//! A bot in OpenClaw style binds to exactly one owner.  The ownership is
//! established during the first launch: the first user who sends the `/pair`
//! command becomes the owner.  Subsequent `/pair` attempts from any other
//! user are rejected.
//!
//! # Persistence
//!
//! The owner's open-platform user ID (`open_id`) is written to
//! `owner.json` in the current working directory.  On restart the file is
//! read back so ownership survives process restarts.
//!
//! # Bootstrap override
//!
//! Set the `REMI_CAT_OWNER_ID` environment variable to pre-configure an
//! owner without going through the pairing flow.  This is useful in CI and
//! containerised deployments.

use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tracing::{info, warn};

const OWNER_FILE: &str = "owner.json";
/// Pairing command that a user must send.
pub const PAIR_COMMAND: &str = "/pair";

// ── Persistence model ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct OwnerRecord {
    owner_id: String,
}

// ── OwnerStatus ───────────────────────────────────────────────────────────────

/// Result of checking whether a user is allowed to send commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerStatus {
    /// The sender is the confirmed owner.
    Owner,
    /// The sender is NOT the owner; bot has an owner already.
    NotOwner,
    /// No owner configured yet — waiting for pairing.
    NeedPairing,
}

// ── OwnerMatcher ──────────────────────────────────────────────────────────────

/// Manages the single-owner binding for the bot.
///
/// Designed to be cheaply cloneable and shared across tasks via `Arc<RwLock<…>>`.
#[derive(Clone)]
pub struct OwnerMatcher {
    inner: Arc<RwLock<Inner>>,
    owner_file: PathBuf,
}

struct Inner {
    owner_id: Option<String>,
}

impl OwnerMatcher {
    /// Create a new `OwnerMatcher`.
    ///
    /// Loads a persisted owner from `owner.json` (current directory) if it
    /// exists. If `REMI_CAT_OWNER_ID` is set in the environment it takes
    /// precedence over the file.
    pub fn load() -> Self {
        let owner_file = PathBuf::from(OWNER_FILE);
        let owner_id = load_owner_id(&owner_file);

        if let Some(ref id) = owner_id {
            info!("Loaded owner_id from storage: {id}");
        } else {
            info!("No owner configured — pairing mode active (send /pair to claim ownership)");
        }

        Self {
            inner: Arc::new(RwLock::new(Inner { owner_id })),
            owner_file,
        }
    }

    /// Check whether `sender_id` is authorised to send commands.
    pub fn check(&self, sender_id: &str) -> OwnerStatus {
        let guard = self.inner.read().expect("lock poisoned");
        match &guard.owner_id {
            None => OwnerStatus::NeedPairing,
            Some(id) if id == sender_id => OwnerStatus::Owner,
            _ => OwnerStatus::NotOwner,
        }
    }

    /// Attempt to pair with `sender_id` as the new owner.
    ///
    /// Only succeeds if no owner is currently configured.
    /// Returns `true` on success, `false` if already paired.
    pub fn try_pair(&self, sender_id: &str) -> bool {
        let mut guard = self.inner.write().expect("lock poisoned");
        if guard.owner_id.is_some() {
            warn!("Pairing attempt rejected — owner already set");
            return false;
        }
        guard.owner_id = Some(sender_id.to_string());
        info!("Owner paired: {sender_id}");
        drop(guard);

        if let Err(e) = persist_owner_id(&self.owner_file, sender_id) {
            warn!("Failed to persist owner_id: {e}");
        }
        true
    }

    /// Return the current owner ID, if set.
    pub fn owner_id(&self) -> Option<String> {
        self.inner.read().expect("lock poisoned").owner_id.clone()
    }

    /// Clear the current owner binding.  The owner file is deleted so the
    /// next `/pair` command registers a new owner.
    pub fn reset(&self) {
        let mut guard = self.inner.write().expect("lock poisoned");
        guard.owner_id = None;
        drop(guard);
        if self.owner_file.exists() {
            if let Err(e) = std::fs::remove_file(&self.owner_file) {
                warn!("Failed to remove owner file: {e}");
            }
        }
        info!("Owner reset");
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Load owner_id from environment variable or JSON file, whichever comes first.
fn load_owner_id(path: &Path) -> Option<String> {
    // Environment variable takes precedence.
    if let Ok(id) = std::env::var("REMI_CAT_OWNER_ID") {
        if !id.is_empty() {
            return Some(id);
        }
    }

    // Read from persisted file.
    let text = std::fs::read_to_string(path).ok()?;
    let record: OwnerRecord = serde_json::from_str(&text).ok()?;
    if record.owner_id.is_empty() {
        None
    } else {
        Some(record.owner_id)
    }
}

/// Write owner_id to a JSON file.
fn persist_owner_id(path: &Path, owner_id: &str) -> std::io::Result<()> {
    let record = OwnerRecord {
        owner_id: owner_id.to_string(),
    };
    let text = serde_json::to_string_pretty(&record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, text)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_matcher(owner_id: Option<&str>) -> OwnerMatcher {
        OwnerMatcher {
            inner: Arc::new(RwLock::new(Inner {
                owner_id: owner_id.map(str::to_string),
            })),
            owner_file: PathBuf::from("/tmp/test_owner.json"),
        }
    }

    #[test]
    fn no_owner_returns_need_pairing() {
        let m = make_matcher(None);
        assert_eq!(m.check("alice"), OwnerStatus::NeedPairing);
    }

    #[test]
    fn owner_recognised() {
        let m = make_matcher(Some("alice"));
        assert_eq!(m.check("alice"), OwnerStatus::Owner);
        assert_eq!(m.check("bob"), OwnerStatus::NotOwner);
    }

    #[test]
    fn pairing_sets_owner() {
        let m = make_matcher(None);
        let ok = m.try_pair("alice");
        assert!(ok);
        assert_eq!(m.check("alice"), OwnerStatus::Owner);
        assert_eq!(m.owner_id(), Some("alice".into()));
    }

    #[test]
    fn double_pair_rejected() {
        let m = make_matcher(None);
        assert!(m.try_pair("alice"));
        assert!(!m.try_pair("bob")); // second attempt rejected
        assert_eq!(m.check("alice"), OwnerStatus::Owner);
    }
}
