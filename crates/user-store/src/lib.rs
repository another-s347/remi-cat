//! `user-store` — UUID-based user identity management.
//!
//! Maintains the mapping from channel-specific user identifiers (Feishu
//! `open_id`, Slack user ID, etc.) to a stable internal UUID.  Multiple
//! channel identities can be linked to a single UUID so the same person is
//! recognised across channels.
//!
//! # Persistence
//!
//! Stored as `users.json` in the daemon data directory:
//!
//! ```json
//! {
//!   "users": [
//!     {
//!       "uuid": "550e8400-e29b-41d4-a716-446655440000",
//!       "channels": [
//!         { "channel": "feishu", "user_id": "ou_xxxx" },
//!         { "channel": "slack",  "user_id": "U012ABCDEF" }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! # Channel pairing tokens
//!
//! The [`PairTokenStore`] is a complementary in-memory store that holds
//! short-lived tokens used by the `/pair-channel` command.  A user sends
//! `/pair-channel` in one channel to generate a token; another user sends
//! `/pair-channel <token>` in a different channel to complete the link.
//! Tokens expire after [`PAIR_TOKEN_TTL_SECS`] seconds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Default lifetime for a pending pair-channel token (5 minutes).
pub const PAIR_TOKEN_TTL_SECS: u64 = 300;

// ── Data model ────────────────────────────────────────────────────────────────

/// A single channel identity tied to a user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelIdentity {
    /// E.g. `"feishu"`, `"slack"`.
    pub channel: String,
    /// The channel-specific user identifier (e.g. Feishu `open_id`).
    pub user_id: String,
}

/// A single user entry in the store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserRecord {
    /// Internal stable UUID assigned by this system.
    pub uuid: String,
    /// All channel identities linked to this user.
    pub channels: Vec<ChannelIdentity>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UserDb {
    users: Vec<UserRecord>,
}

// ── UserStore ─────────────────────────────────────────────────────────────────

/// Thread-safe user identity store.
///
/// Designed to be cheaply cloneable via the inner `Arc`.
#[derive(Clone)]
pub struct UserStore {
    inner: Arc<RwLock<UserDb>>,
    path: PathBuf,
}

impl UserStore {
    /// Load from `path`, creating an empty store if the file does not exist.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let db = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading user store {path:?}"))?;
            serde_json::from_str::<UserDb>(&raw)
                .with_context(|| format!("parsing user store {path:?}"))?
        } else {
            debug!(path = ?path, "user store not found — starting empty");
            UserDb::default()
        };
        info!(path = ?path, users = db.users.len(), "user store loaded");
        Ok(Self {
            inner: Arc::new(RwLock::new(db)),
            path,
        })
    }

    /// Resolve `(channel, user_id)` to our UUID, **creating a new user** if
    /// no match is found.
    pub fn resolve_or_create(&self, channel: &str, user_id: &str) -> String {
        // Fast path: read lock.
        {
            let db = self.inner.read().expect("lock poisoned");
            if let Some(uuid) = find_uuid(&db, channel, user_id) {
                return uuid;
            }
        }

        // Slow path: write lock — create new user.
        let mut db = self.inner.write().expect("lock poisoned");
        // Re-check under write lock (another thread may have inserted).
        if let Some(uuid) = find_uuid(&db, channel, user_id) {
            return uuid;
        }

        let uuid = uuid::Uuid::new_v4().to_string();
        db.users.push(UserRecord {
            uuid: uuid.clone(),
            channels: vec![ChannelIdentity {
                channel: channel.to_string(),
                user_id: user_id.to_string(),
            }],
        });
        info!(channel, user_id, %uuid, "new user created");
        drop(db);
        if let Err(e) = self.save() {
            warn!("failed to persist user store: {e:#}");
        }
        uuid
    }

    /// Link two channel identities together under one UUID.
    ///
    /// If identity A is already known, add identity B to its record (or
    /// vice-versa).  If both are known but to *different* users, the two
    /// records are **merged** (B's record is absorbed into A's).  If neither
    /// is known, a fresh user is created with both identities.
    ///
    /// Returns the UUID that now owns both identities.
    pub fn link(
        &self,
        channel_a: &str,
        user_id_a: &str,
        channel_b: &str,
        user_id_b: &str,
    ) -> Result<String> {
        let mut db = self.inner.write().expect("lock poisoned");

        let idx_a = find_user_index(&db, channel_a, user_id_a);
        let idx_b = find_user_index(&db, channel_b, user_id_b);

        let uuid = match (idx_a, idx_b) {
            (Some(ia), Some(ib)) if ia == ib => {
                // Already the same user.
                db.users[ia].uuid.clone()
            }
            (Some(ia), Some(ib)) => {
                // Merge: absorb the record at the higher index into the lower one.
                let (lo, hi) = if ia < ib { (ia, ib) } else { (ib, ia) };
                // Clone the channels from the record that will be removed.
                let hi_channels: Vec<ChannelIdentity> = db.users[hi].channels.clone();
                for ch in hi_channels {
                    if !db.users[lo].channels.contains(&ch) {
                        db.users[lo].channels.push(ch);
                    }
                }
                let uuid = db.users[lo].uuid.clone();
                db.users.remove(hi);
                info!(uuid = %uuid, "merged two user records");
                uuid
            }
            (Some(ia), None) => {
                // Add identity B to existing user A.
                let identity_b = ChannelIdentity {
                    channel: channel_b.to_string(),
                    user_id: user_id_b.to_string(),
                };
                if !db.users[ia].channels.contains(&identity_b) {
                    db.users[ia].channels.push(identity_b);
                }
                let uuid = db.users[ia].uuid.clone();
                info!(uuid = %uuid, channel_b, user_id_b, "linked new channel to existing user");
                uuid
            }
            (None, Some(ib)) => {
                // Add identity A to existing user B.
                let identity_a = ChannelIdentity {
                    channel: channel_a.to_string(),
                    user_id: user_id_a.to_string(),
                };
                if !db.users[ib].channels.contains(&identity_a) {
                    db.users[ib].channels.push(identity_a);
                }
                let uuid = db.users[ib].uuid.clone();
                info!(uuid = %uuid, channel_a, user_id_a, "linked new channel to existing user");
                uuid
            }
            (None, None) => {
                // Neither is known — create a new joint user.
                let uuid = uuid::Uuid::new_v4().to_string();
                db.users.push(UserRecord {
                    uuid: uuid.clone(),
                    channels: vec![
                        ChannelIdentity {
                            channel: channel_a.to_string(),
                            user_id: user_id_a.to_string(),
                        },
                        ChannelIdentity {
                            channel: channel_b.to_string(),
                            user_id: user_id_b.to_string(),
                        },
                    ],
                });
                info!(%uuid, channel_a, user_id_a, channel_b, user_id_b, "created new joint user");
                uuid
            }
        };

        drop(db);
        self.save()?;
        Ok(uuid)
    }

    /// Remove a channel identity mapping.  Returns `true` if the mapping
    /// existed.  If removing the identity leaves the user with no channels,
    /// the user record is deleted entirely.
    pub fn unlink(&self, channel: &str, user_id: &str) -> Result<bool> {
        let mut db = self.inner.write().expect("lock poisoned");
        let Some(idx) = find_user_index(&db, channel, user_id) else {
            return Ok(false);
        };
        db.users[idx]
            .channels
            .retain(|c| !(c.channel == channel && c.user_id == user_id));
        if db.users[idx].channels.is_empty() {
            let uuid = db.users.remove(idx).uuid;
            info!(%uuid, "user deleted (no remaining channels)");
        }
        drop(db);
        self.save()?;
        Ok(true)
    }

    /// Delete all channel identities for the given UUID.
    /// Returns `true` if the user existed.
    pub fn delete_by_uuid(&self, uuid: &str) -> Result<bool> {
        let mut db = self.inner.write().expect("lock poisoned");
        let pos = db.users.iter().position(|u| u.uuid == uuid);
        let Some(idx) = pos else { return Ok(false) };
        db.users.remove(idx);
        drop(db);
        self.save()?;
        info!(%uuid, "user deleted by uuid");
        Ok(true)
    }

    /// Return all user records.
    pub fn list(&self) -> Vec<UserRecord> {
        self.inner.read().expect("lock poisoned").users.clone()
    }

    // ── Persistence ───────────────────────────────────────────────────────────

    fn save(&self) -> Result<()> {
        let db = self.inner.read().expect("lock poisoned");
        let json = serde_json::to_string_pretty(&*db).context("serialising user store")?;
        drop(db);
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .with_context(|| format!("writing user store tmp {tmp:?}"))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming user store {tmp:?} → {:?}", self.path))?;
        debug!(path = ?self.path, "user store saved");
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn find_uuid(db: &UserDb, channel: &str, user_id: &str) -> Option<String> {
    db.users.iter().find_map(|u| {
        u.channels
            .iter()
            .any(|c| c.channel == channel && c.user_id == user_id)
            .then(|| u.uuid.clone())
    })
}

fn find_user_index(db: &UserDb, channel: &str, user_id: &str) -> Option<usize> {
    db.users.iter().position(|u| {
        u.channels
            .iter()
            .any(|c| c.channel == channel && c.user_id == user_id)
    })
}

// ── PairTokenStore ────────────────────────────────────────────────────────────

/// Pending `/pair-channel` token entry.
#[derive(Debug, Clone)]
pub struct PendingPair {
    /// Channel that initiated the pairing (e.g. `"feishu"`).
    pub channel: String,
    /// Channel-specific user ID of the initiator.
    pub user_id: String,
    /// When the token was created.
    pub created_at: Instant,
}

/// In-memory store for short-lived channel-pairing tokens.
///
/// Tokens are 6-character upper-case alphanumeric strings and expire after
/// [`PAIR_TOKEN_TTL_SECS`] seconds.
#[derive(Clone, Default)]
pub struct PairTokenStore {
    inner: Arc<RwLock<HashMap<String, PendingPair>>>,
}

impl PairTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generate and store a new pairing token for `(channel, user_id)`.
    /// Returns the generated token string.
    pub fn create(&self, channel: &str, user_id: &str) -> String {
        use rand::Rng as _;
        let token: String = rand::thread_rng()
            .sample_iter(rand::distributions::Alphanumeric)
            .take(6)
            .map(|c| (c as char).to_ascii_uppercase())
            .collect();

        let mut store = self.inner.write().expect("lock poisoned");
        // Remove any expired tokens while we're in here.
        store.retain(|_, v| v.created_at.elapsed() < Duration::from_secs(PAIR_TOKEN_TTL_SECS));
        store.insert(
            token.clone(),
            PendingPair {
                channel: channel.to_string(),
                user_id: user_id.to_string(),
                created_at: Instant::now(),
            },
        );
        token
    }

    /// Consume a token.  Returns the original [`PendingPair`] if the token is
    /// valid and not expired, or `None` otherwise.
    pub fn consume(&self, token: &str) -> Option<PendingPair> {
        let mut store = self.inner.write().expect("lock poisoned");
        let entry = store.remove(&token.to_uppercase())?;
        if entry.created_at.elapsed() >= Duration::from_secs(PAIR_TOKEN_TTL_SECS) {
            return None;
        }
        Some(entry)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_store() -> UserStore {
        let path = std::env::temp_dir()
            .join(format!("test_users_{}.json", uuid::Uuid::new_v4()));
        UserStore {
            inner: Arc::new(RwLock::new(UserDb::default())),
            path,
        }
    }

    #[test]
    fn resolve_or_create_creates_new_user() {
        let store = in_memory_store();
        let uuid = store.resolve_or_create("feishu", "ou_alice");
        assert!(!uuid.is_empty());
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn resolve_or_create_is_idempotent() {
        let store = in_memory_store();
        let uuid1 = store.resolve_or_create("feishu", "ou_alice");
        let uuid2 = store.resolve_or_create("feishu", "ou_alice");
        assert_eq!(uuid1, uuid2);
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn different_channel_ids_get_different_uuids_initially() {
        let store = in_memory_store();
        let uuid_alice = store.resolve_or_create("feishu", "ou_alice");
        let uuid_bob = store.resolve_or_create("feishu", "ou_bob");
        assert_ne!(uuid_alice, uuid_bob);
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn link_two_new_identities() {
        let store = in_memory_store();
        let uuid = store
            .link("feishu", "ou_alice", "slack", "U_alice")
            .unwrap();
        assert!(!uuid.is_empty());
        let users = store.list();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].channels.len(), 2);
    }

    #[test]
    fn link_adds_to_existing_user() {
        let store = in_memory_store();
        let uuid_a = store.resolve_or_create("feishu", "ou_alice");
        let uuid_linked = store
            .link("feishu", "ou_alice", "slack", "U_alice")
            .unwrap();
        assert_eq!(uuid_a, uuid_linked);
        let users = store.list();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].channels.len(), 2);
    }

    #[test]
    fn link_merges_two_existing_users() {
        let store = in_memory_store();
        let uuid_a = store.resolve_or_create("feishu", "ou_alice");
        let uuid_b = store.resolve_or_create("slack", "U_alice");
        assert_ne!(uuid_a, uuid_b);
        // Merge: both should now be the same user.
        let merged_uuid = store
            .link("feishu", "ou_alice", "slack", "U_alice")
            .unwrap();
        let users = store.list();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].uuid, merged_uuid);
        assert_eq!(users[0].channels.len(), 2);
    }

    #[test]
    fn unlink_removes_channel() {
        let store = in_memory_store();
        store
            .link("feishu", "ou_alice", "slack", "U_alice")
            .unwrap();
        let removed = store.unlink("slack", "U_alice").unwrap();
        assert!(removed);
        let users = store.list();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].channels.len(), 1);
    }

    #[test]
    fn unlink_last_channel_deletes_user() {
        let store = in_memory_store();
        store.resolve_or_create("feishu", "ou_alice");
        let removed = store.unlink("feishu", "ou_alice").unwrap();
        assert!(removed);
        assert!(store.list().is_empty());
    }

    #[test]
    fn delete_by_uuid_works() {
        let store = in_memory_store();
        let uuid = store.resolve_or_create("feishu", "ou_alice");
        let deleted = store.delete_by_uuid(&uuid).unwrap();
        assert!(deleted);
        assert!(store.list().is_empty());
    }

    #[test]
    fn pair_token_round_trip() {
        let pts = PairTokenStore::new();
        let token = pts.create("feishu", "ou_alice");
        assert_eq!(token.len(), 6);
        let pending = pts.consume(&token).expect("token should be valid");
        assert_eq!(pending.channel, "feishu");
        assert_eq!(pending.user_id, "ou_alice");
        // Second consume should return None.
        assert!(pts.consume(&token).is_none());
    }

    #[test]
    fn pair_token_is_case_insensitive() {
        let pts = PairTokenStore::new();
        let token = pts.create("feishu", "ou_alice");
        let lower = token.to_lowercase();
        let pending = pts.consume(&lower).expect("should match regardless of case");
        assert_eq!(pending.user_id, "ou_alice");
    }
}
