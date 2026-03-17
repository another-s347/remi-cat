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
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tracing::{info, warn};

const OWNER_FILE: &str = "owner.json";
const BLACKLIST_FILE: &str = "blacklist.json";
/// Pairing command that a user must send.
pub const PAIR_COMMAND: &str = "/pair";

// ── Persistence model ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct OwnerRecord {
    owner_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BlacklistRecord {
    blocked_ids: Vec<String>,
}

/// Load a list of extra allowed UUIDs from `REMI_CAT_ALLOWED_IDS` (comma-separated).
fn load_allowed_ids() -> std::collections::HashSet<String> {
    std::env::var("REMI_CAT_ALLOWED_IDS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// ── OwnerStatus ───────────────────────────────────────────────────────────────

/// Result of checking whether a user is allowed to send commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerStatus {
    /// The sender has been blocked and cannot talk to the bot.
    Banned,
    /// The sender is the confirmed owner.
    Owner,
    /// The sender is NOT the owner; bot has an owner already.
    NotOwner,
    /// No owner configured yet — waiting for pairing.
    NeedPairing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BanResult {
    Added,
    AlreadyBanned,
    ProtectedOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnbanResult {
    Removed,
    NotBanned,
}

// ── OwnerMatcher ──────────────────────────────────────────────────────────────

/// Manages the single-owner binding for the bot.
///
/// Designed to be cheaply cloneable and shared across tasks via `Arc<RwLock<…>>`.
#[derive(Clone)]
pub struct OwnerMatcher {
    inner: Arc<RwLock<Inner>>,
    owner_file: PathBuf,
    blacklist_file: PathBuf,
}

struct Inner {
    owner_id: Option<String>,
    /// Extra UUIDs allowed to use the bot (in addition to the owner).
    allowed_ids: HashSet<String>,
    blocked_ids: HashSet<String>,
}

impl OwnerMatcher {
    /// Create a new `OwnerMatcher`.
    ///
    /// Loads a persisted owner from `owner.json` (current directory) if it
    /// exists. If `REMI_CAT_OWNER_ID` is set in the environment it takes
    /// precedence over the file.
    pub fn load() -> Self {
        let owner_file = PathBuf::from(OWNER_FILE);
        let blacklist_file = PathBuf::from(BLACKLIST_FILE);
        let owner_id = load_owner_id(&owner_file);
        let blocked_ids = load_blacklist(&blacklist_file);

        if let Some(ref id) = owner_id {
            info!("Loaded owner_id from storage: {id}");
        } else {
            info!("No owner configured — pairing mode active (send /pair to claim ownership)");
        }

        let allowed_ids = load_allowed_ids();
        if !allowed_ids.is_empty() {
            info!("Allowed IDs (REMI_CAT_ALLOWED_IDS): {:?}", allowed_ids);
        }
        if !blocked_ids.is_empty() {
            info!("Blocked IDs loaded: {:?}", blocked_ids);
        }

        Self {
            inner: Arc::new(RwLock::new(Inner {
                owner_id,
                allowed_ids,
                blocked_ids,
            })),
            owner_file,
            blacklist_file,
        }
    }

    /// Check whether `sender_id` is authorised to send commands.
    pub fn check(&self, sender_id: &str) -> OwnerStatus {
        let guard = self.inner.read().expect("lock poisoned");
        if guard.blocked_ids.contains(sender_id) {
            return OwnerStatus::Banned;
        }
        // Extra allowed IDs are treated as Owner-level access.
        if guard.allowed_ids.contains(sender_id) {
            return OwnerStatus::Owner;
        }
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

    pub fn is_owner(&self, sender_id: &str) -> bool {
        matches!(self.check(sender_id), OwnerStatus::Owner)
    }

    pub fn is_banned(&self, sender_id: &str) -> bool {
        matches!(self.check(sender_id), OwnerStatus::Banned)
    }

    pub fn list_banned(&self) -> Vec<String> {
        let guard = self.inner.read().expect("lock poisoned");
        let mut ids: Vec<String> = guard.blocked_ids.iter().cloned().collect();
        ids.sort();
        ids
    }

    pub fn ban(&self, sender_id: &str) -> std::io::Result<BanResult> {
        let mut guard = self.inner.write().expect("lock poisoned");
        if guard.allowed_ids.contains(sender_id) || guard.owner_id.as_deref() == Some(sender_id) {
            return Ok(BanResult::ProtectedOwner);
        }
        if !guard.blocked_ids.insert(sender_id.to_string()) {
            return Ok(BanResult::AlreadyBanned);
        }
        persist_blacklist(&self.blacklist_file, &guard.blocked_ids)?;
        info!(target = sender_id, "user added to blacklist");
        Ok(BanResult::Added)
    }

    pub fn unban(&self, sender_id: &str) -> std::io::Result<UnbanResult> {
        let mut guard = self.inner.write().expect("lock poisoned");
        if !guard.blocked_ids.remove(sender_id) {
            return Ok(UnbanResult::NotBanned);
        }
        persist_blacklist(&self.blacklist_file, &guard.blocked_ids)?;
        info!(target = sender_id, "user removed from blacklist");
        Ok(UnbanResult::Removed)
    }

    pub fn migrate_blacklist(&self, old_ids: &[String], new_id: &str) -> std::io::Result<bool> {
        let mut guard = self.inner.write().expect("lock poisoned");
        let mut removed_any = false;

        for old_id in old_ids {
            if old_id != new_id && guard.blocked_ids.remove(old_id) {
                removed_any = true;
            }
        }

        let should_block_new = removed_any
            && !guard.allowed_ids.contains(new_id)
            && guard.owner_id.as_deref() != Some(new_id);
        let inserted_new = if should_block_new {
            guard.blocked_ids.insert(new_id.to_string())
        } else {
            false
        };

        if !removed_any && !inserted_new {
            return Ok(false);
        }

        persist_blacklist(&self.blacklist_file, &guard.blocked_ids)?;
        info!(new_id, old_ids = ?old_ids, "blacklist migrated across merged identities");
        Ok(true)
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

fn load_blacklist(path: &Path) -> HashSet<String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return HashSet::new(),
    };
    let record: BlacklistRecord = match serde_json::from_str(&text) {
        Ok(record) => record,
        Err(_) => return HashSet::new(),
    };
    record
        .blocked_ids
        .into_iter()
        .filter(|id| !id.is_empty())
        .collect()
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

fn persist_blacklist(path: &Path, blocked_ids: &HashSet<String>) -> std::io::Result<()> {
    let mut ids: Vec<String> = blocked_ids.iter().cloned().collect();
    ids.sort();
    let record = BlacklistRecord { blocked_ids: ids };
    let text = serde_json::to_string_pretty(&record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "matcher_{name}_{}_{}.json",
            std::process::id(),
            stamp
        ))
    }

    fn make_matcher(owner_id: Option<&str>) -> OwnerMatcher {
        OwnerMatcher {
            inner: Arc::new(RwLock::new(Inner {
                owner_id: owner_id.map(str::to_string),
                allowed_ids: HashSet::new(),
                blocked_ids: HashSet::new(),
            })),
            owner_file: temp_path("owner"),
            blacklist_file: temp_path("blacklist"),
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
    fn blacklisted_user_is_blocked() {
        let m = make_matcher(Some("alice"));
        assert_eq!(m.ban("bob").unwrap(), BanResult::Added);
        assert_eq!(m.check("bob"), OwnerStatus::Banned);
        assert!(m.is_banned("bob"));
    }

    #[test]
    fn cannot_ban_owner() {
        let m = make_matcher(Some("alice"));
        assert_eq!(m.ban("alice").unwrap(), BanResult::ProtectedOwner);
        assert_eq!(m.check("alice"), OwnerStatus::Owner);
    }

    #[test]
    fn unban_removes_from_blacklist() {
        let m = make_matcher(Some("alice"));
        assert_eq!(m.ban("bob").unwrap(), BanResult::Added);
        assert_eq!(m.unban("bob").unwrap(), UnbanResult::Removed);
        assert_eq!(m.check("bob"), OwnerStatus::NotOwner);
    }

    #[test]
    fn migrate_blacklist_moves_block_to_merged_uuid() {
        let m = make_matcher(Some("alice"));
        assert_eq!(m.ban("bob-old").unwrap(), BanResult::Added);
        assert!(m
            .migrate_blacklist(&["bob-old".into(), "bob-new".into()], "bob-new")
            .unwrap());
        assert!(!m.is_banned("bob-old"));
        assert!(m.is_banned("bob-new"));
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
