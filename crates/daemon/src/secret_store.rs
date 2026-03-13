//! Persistent KV secret store — lives entirely on the Daemon host, never
//! mounted into the Agent container.
//!
//! Secrets are stored as a flat JSON object (`{"KEY": "value", …}`) encrypted
//! with **AES-256-GCM** at a path configured via the `SECRET_STORE_PATH`
//! environment variable, falling back to `<daemon-exe-dir>/secrets.json`.
//!
//! # Key management
//!
//! A random 256-bit encryption key is generated on first use and stored next
//! to the data file at `<path>.key` with file-system permissions restricted to
//! the owner (`0600` on Unix).  Keeping the key separate from the ciphertext
//! means exfiltrating the data file alone is not sufficient to recover secrets.
//!
//! # File format
//!
//! `[12-byte random nonce] [AES-256-GCM ciphertext + 16-byte auth tag]`
//!
//! # Atomicity
//!
//! Writes go to `<path>.tmp` and are then renamed over the target file so an
//! interrupted write never corrupts the store.
//!
//! # Migration
//!
//! If an existing plaintext JSON file is found it is automatically encrypted
//! in place on the first load, with no data loss.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Key, Nonce,
};
use anyhow::{Context, Result};
use tracing::{debug, info, warn};

// ── Persisted format ─────────────────────────────────────────────────────────

/// On-disk (post-decryption) representation of the secret store.
///
/// Versioned via optional fields so old encrypted stores (flat `{"KEY":"val"}`)
/// can be loaded and automatically migrated.
#[derive(serde::Serialize, serde::Deserialize, Default, Clone, Debug)]
#[serde(deny_unknown_fields)]
struct StoreData {
    /// All secret key-value pairs (both user and system secrets).
    #[serde(default)]
    entries: HashMap<String, String>,
    /// Keys whose values are **not** forwarded to the Agent via `SecretsSync`.
    /// System secrets are only accessible in daemon code (e.g. Feishu credentials).
    #[serde(default)]
    system_keys: HashSet<String>,
}

// ── SecretStore ───────────────────────────────────────────────────────────────

/// In-memory KV store backed by an AES-256-GCM encrypted file on disk.
pub struct SecretStore {
    data: StoreData,
    path: PathBuf,
    cipher: Aes256Gcm,
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
    /// Existing plaintext JSON files are migrated to encrypted format
    /// transparently on first load.
    pub fn load(path: PathBuf) -> Result<Self> {
        let cipher = load_or_create_key(&path)?;
        let data = if path.exists() {
            let raw = std::fs::read(&path)
                .with_context(|| format!("reading secret store {path:?}"))?;
            if looks_like_plaintext_json(&raw) {
                // ── Migration: plaintext → encrypted ─────────────────────
                warn!(
                    path = ?path,
                    "secret store is plaintext — migrating to encrypted format"
                );
                // Try new StoreData format first, fall back to old flat HashMap.
                let data = serde_json::from_slice::<StoreData>(&raw).unwrap_or_else(|_| {
                    let entries: HashMap<String, String> =
                        serde_json::from_slice(&raw).unwrap_or_default();
                    StoreData { entries, system_keys: HashSet::new() }
                });
                // Write encrypted version immediately.
                let store = Self { data: data.clone(), path: path.clone(), cipher };
                store.save()?;
                return Ok(store);
            } else {
                decrypt_entries(&cipher, &raw, &path)?
            }
        } else {
            info!("secret store not found at {path:?} — starting empty");
            StoreData::default()
        };
        debug!(path = ?path, count = data.entries.len(), "secret store loaded");
        Ok(Self { data, path, cipher })
    }

    // ── Mutations ─────────────────────────────────────────────────────────

    /// Insert or update a key, preserving any existing `system` flag.  Empty values are rejected.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> Result<()> {
        let key = key.into();
        let value = value.into();
        self.validate_kv(&key, &value)?;
        // Preserve existing system flag — do not demote a system key.
        self.data.entries.insert(key, value);
        self.save()
    }

    /// Insert or update a key and mark it as a **system secret**.
    ///
    /// System secrets are never forwarded to the Agent via `SecretsSync`;
    /// they are only accessible inside daemon code.
    pub fn set_system(&mut self, key: impl Into<String>, value: impl Into<String>) -> Result<()> {
        let key = key.into();
        let value = value.into();
        self.validate_kv(&key, &value)?;
        self.data.system_keys.insert(key.clone());
        self.data.entries.insert(key, value);
        self.save()
    }

    fn validate_kv(&self, key: &str, value: &str) -> Result<()> {
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
        Ok(())
    }

    /// Mark an existing key as a system secret.  No-op if the key does not exist.
    pub fn mark_system(&mut self, key: &str) -> Result<()> {
        if self.data.entries.contains_key(key) {
            self.data.system_keys.insert(key.to_string());
            self.save()?;
        }
        Ok(())
    }

    /// Remove the system flag from a key.  No-op if not a system key.
    pub fn unmark_system(&mut self, key: &str) -> Result<()> {
        self.data.system_keys.remove(key);
        self.save()
    }

    /// Remove a key.  No-ops silently if the key does not exist.
    pub fn delete(&mut self, key: &str) -> Result<()> {
        self.data.entries.remove(key);
        self.data.system_keys.remove(key);
        self.save()
    }

    // ── Readers ───────────────────────────────────────────────────────────

    /// Look up a single secret by key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.entries.get(key).map(String::as_str)
    }

    /// Sorted list of all keys (both user and system).
    pub fn keys(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.data.entries.keys().map(String::as_str).collect();
        v.sort();
        v
    }

    /// Whether `key` is a system secret (not forwarded to the Agent).
    pub fn is_system(&self, key: &str) -> bool {
        self.data.system_keys.contains(key)
    }

    /// All entries including system secrets — used for admin display.
    pub fn all_entries(&self) -> HashMap<String, String> {
        self.data.entries.clone()
    }

    /// Non-system entries only — forwarded to the Agent via `SecretsSync`.
    pub fn agent_entries(&self) -> HashMap<String, String> {
        self.data.entries.iter()
            .filter(|(k, _)| !self.data.system_keys.contains(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    // ── Persistence ───────────────────────────────────────────────────────

    fn save(&self) -> Result<()> {
        let json =
            serde_json::to_string(&self.data).context("serialising secret store")?;

        // Encrypt: random nonce || ciphertext+tag
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, json.as_bytes())
            .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

        let mut blob = Vec::with_capacity(12 + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);

        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &blob)
            .with_context(|| format!("writing secret store tmp {tmp:?}"))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("renaming secret store {tmp:?} → {:?}", self.path))?;
        debug!(path = ?self.path, count = self.data.entries.len(), "secret store saved");
        Ok(())
    }
}

// ── Key management ────────────────────────────────────────────────────────────

/// Path of the encryption key file alongside the data file.
fn key_path(data_path: &Path) -> PathBuf {
    let mut p = data_path.to_path_buf();
    let stem = p
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    p.set_file_name(format!("{stem}.key"));
    p
}

/// Load an existing 256-bit key, or generate and persist a new one.
fn load_or_create_key(data_path: &Path) -> Result<Aes256Gcm> {
    let kp = key_path(data_path);
    let key_bytes: [u8; 32] = if kp.exists() {
        let raw = std::fs::read(&kp)
            .with_context(|| format!("reading secret store key {kp:?}"))?;
        raw.try_into()
            .map_err(|_| anyhow::anyhow!("key file {kp:?} must be exactly 32 bytes"))?
    } else {
        // Generate a fresh random key.
        let key = Aes256Gcm::generate_key(OsRng);
        let bytes: [u8; 32] = key.into();
        write_key_file(&kp, &bytes)?;
        info!(path = ?kp, "generated new secret store encryption key");
        bytes
    };
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    Ok(Aes256Gcm::new(key))
}

/// Write `key_bytes` to `path` with owner-only permissions (0600 on Unix).
fn write_key_file(path: &Path, key_bytes: &[u8; 32]) -> Result<()> {
    // Write first (creates the file).
    std::fs::write(path, key_bytes)
        .with_context(|| format!("writing key file {path:?}"))?;
    // Restrict permissions on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on key file {path:?}"))?;
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return `true` if `data` looks like a plaintext UTF-8 JSON object.
///
/// The encrypted format starts with a 12-byte random nonce, which has a
/// negligible probability of beginning with an ASCII `{`, space, or newline.
fn looks_like_plaintext_json(data: &[u8]) -> bool {
    matches!(data.first(), Some(&b'{' | &b' ' | &b'\t' | &b'\n' | &b'\r'))
}

/// Decrypt the on-disk blob and parse as [`StoreData`].
///
/// Automatically migrates old stores that were a plain `{"KEY":"value"}` object.
fn decrypt_entries(
    cipher: &Aes256Gcm,
    blob: &[u8],
    path: &Path,
) -> Result<StoreData> {
    if blob.len() < 28 {
        anyhow::bail!("secret store {path:?} is too short to be valid");
    }
    let nonce = Nonce::from_slice(&blob[..12]);
    let plaintext = cipher
        .decrypt(nonce, &blob[12..])
        .map_err(|_| anyhow::anyhow!(
            "failed to decrypt secret store {path:?} — key mismatch or data corrupted"
        ))?;
    // Try new StoreData format first; fall back to old flat HashMap for existing stores.
    if let Ok(data) = serde_json::from_slice::<StoreData>(&plaintext) {
        return Ok(data);
    }
    let entries = serde_json::from_slice::<HashMap<String, String>>(&plaintext)
        .with_context(|| format!("parsing decrypted secret store {path:?}"))?;
    Ok(StoreData { entries, system_keys: HashSet::new() })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Create a temporary directory for the store and return a path inside it.
    fn tmp_store_path() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Keep the dir alive for the duration of the test by leaking it.
        let path = dir.path().join("secrets.json");
        std::mem::forget(dir);
        path
    }

    #[test]
    fn round_trip_empty_store() {
        let path = tmp_store_path();
        let mut store = SecretStore::load(path.clone()).unwrap();
        store.set("FOO", "bar").unwrap();
        store.set("BAZ", "qux").unwrap();
        drop(store);

        // Reload — must decrypt and return the same entries.
        let store2 = SecretStore::load(path).unwrap();
        assert_eq!(store2.get("FOO"), Some("bar"));
        assert_eq!(store2.get("BAZ"), Some("qux"));
    }

    #[test]
    fn key_file_has_restricted_permissions() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let path = tmp_store_path();
            SecretStore::load(path.clone()).unwrap();
            let kp = super::key_path(&path);
            let mode = std::fs::metadata(&kp).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "key file must be 0600");
        }
    }

    #[test]
    fn migrate_plaintext_json() {
        let path = tmp_store_path();
        // Write a plaintext JSON file (old format).
        let plaintext = r#"{"OPENAI_API_KEY":"sk-test","EXA_API_KEY":"exa-test"}"#;
        std::fs::write(&path, plaintext).unwrap();

        // Load should migrate transparently.
        let store = SecretStore::load(path.clone()).unwrap();
        assert_eq!(store.get("OPENAI_API_KEY"), Some("sk-test"));
        assert_eq!(store.get("EXA_API_KEY"), Some("exa-test"));

        // The on-disk file must no longer be plaintext JSON after migration.
        let on_disk = std::fs::read(&path).unwrap();
        assert!(
            !looks_like_plaintext_json(&on_disk),
            "file should be encrypted after migration"
        );
    }

    #[test]
    fn delete_removes_key() {
        let path = tmp_store_path();
        let mut store = SecretStore::load(path.clone()).unwrap();
        store.set("KEY_A", "value_a").unwrap();
        store.delete("KEY_A").unwrap();
        drop(store);

        let store2 = SecretStore::load(path).unwrap();
        assert_eq!(store2.get("KEY_A"), None);
    }

    #[test]
    fn get_returns_none_for_missing_key() {
        let path = tmp_store_path();
        let store = SecretStore::load(path).unwrap();
        assert_eq!(store.get("NONEXISTENT"), None);
    }

    #[test]
    fn looks_like_plaintext_json_detection() {
        assert!(looks_like_plaintext_json(b"{\"key\":\"val\"}"));
        assert!(looks_like_plaintext_json(b" {\"key\":\"val\"}"));
        assert!(!looks_like_plaintext_json(b"\x01\x02random bytes"));
        assert!(!looks_like_plaintext_json(b""));
    }
}
