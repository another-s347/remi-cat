use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::runtime_config::{load_dotenv_pairs, write_dotenv_pairs};

const KEYRING_SERVICE: &str = "remi-cat";
const KEYRING_INDEX_USER: &str = "__remi_cat_secret_index";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretBackend {
    Dotenv { path: PathBuf },
    Keyring { service: String },
}

#[derive(Debug, Clone)]
pub struct SecretStore {
    backend: SecretBackend,
}

impl SecretStore {
    pub fn from_env() -> Self {
        let backend = std::env::var("REMI_SECRET_BACKEND")
            .unwrap_or_else(|_| "dotenv".to_string())
            .trim()
            .to_ascii_lowercase();
        match backend.as_str() {
            "keyring" | "system" | "system_keyring" => Self {
                backend: SecretBackend::Keyring {
                    service: std::env::var("REMI_SECRET_KEYRING_SERVICE")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| KEYRING_SERVICE.to_string()),
                },
            },
            _ => Self {
                backend: SecretBackend::Dotenv {
                    path: std::env::var("REMI_SECRET_DOTENV_PATH")
                        .map(PathBuf::from)
                        .unwrap_or_else(|_| PathBuf::from(".env")),
                },
            },
        }
    }

    pub fn backend_label(&self) -> String {
        match &self.backend {
            SecretBackend::Dotenv { path } => format!("dotenv:{}", path.display()),
            SecretBackend::Keyring { service } => format!("keyring:{service}"),
        }
    }

    pub fn entries(&self) -> Result<BTreeMap<String, String>> {
        match &self.backend {
            SecretBackend::Dotenv { path } => load_dotenv_pairs(path),
            SecretBackend::Keyring { service } => keyring_entries(service),
        }
    }

    pub fn keys(&self) -> Result<Vec<String>> {
        Ok(self.entries()?.into_keys().collect())
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        match &self.backend {
            SecretBackend::Dotenv { .. } => Ok(self.entries()?.remove(key)),
            SecretBackend::Keyring { service } => keyring_get(service, key),
        }
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        validate_secret_key(key)?;
        if value.is_empty() {
            anyhow::bail!("secret value must not be empty");
        }
        match &self.backend {
            SecretBackend::Dotenv { path } => {
                let mut pairs = load_dotenv_pairs(path)?;
                pairs.insert(key.to_string(), value.to_string());
                write_dotenv_pairs(path, &pairs)
            }
            SecretBackend::Keyring { service } => keyring_set(service, key, value),
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        validate_secret_key(key)?;
        match &self.backend {
            SecretBackend::Dotenv { path } => {
                let mut pairs = load_dotenv_pairs(path)?;
                pairs.remove(key);
                write_dotenv_pairs(path, &pairs)
            }
            SecretBackend::Keyring { service } => keyring_delete(service, key),
        }
    }
}

pub fn apply_entries_to_env(entries: &BTreeMap<String, String>) {
    for (key, value) in entries {
        unsafe {
            std::env::set_var(key, value);
        }
    }
}

pub fn redaction_entries(entries: &BTreeMap<String, String>) -> HashMap<String, String> {
    entries
        .iter()
        .filter(|(key, value)| is_secret_like_key(key) && value.len() >= 4)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn validate_secret_key(key: &str) -> Result<()> {
    if key.is_empty() {
        anyhow::bail!("secret key must not be empty");
    }
    if key.starts_with(|ch: char| ch.is_ascii_digit())
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        anyhow::bail!("secret key `{key}` is not a valid environment variable name");
    }
    Ok(())
}

fn is_secret_like_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    ["KEY", "SECRET", "TOKEN", "PASSWORD", "CREDENTIAL"]
        .iter()
        .any(|marker| key.contains(marker))
}

fn keyring_entry(service: &str, key: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service, key).context("opening system keyring entry")
}

fn keyring_get(service: &str, key: &str) -> Result<Option<String>> {
    match keyring_entry(service, key)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err).context("reading system keyring secret"),
    }
}

fn keyring_set(service: &str, key: &str, value: &str) -> Result<()> {
    keyring_entry(service, key)?
        .set_password(value)
        .context("writing system keyring secret")?;
    let mut keys = keyring_index(service)?;
    if !keys.iter().any(|item| item == key) {
        keys.push(key.to_string());
        keys.sort();
        write_keyring_index(service, &keys)?;
    }
    Ok(())
}

fn keyring_delete(service: &str, key: &str) -> Result<()> {
    match keyring_entry(service, key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(err) => return Err(err).context("deleting system keyring secret"),
    }
    let mut keys = keyring_index(service)?;
    keys.retain(|item| item != key);
    write_keyring_index(service, &keys)
}

fn keyring_entries(service: &str) -> Result<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    for key in keyring_index(service)? {
        if let Some(value) = keyring_get(service, &key)? {
            entries.insert(key, value);
        }
    }
    Ok(entries)
}

fn keyring_index(service: &str) -> Result<Vec<String>> {
    let Some(raw) = keyring_get(service, KEYRING_INDEX_USER)? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).context("parsing system keyring secret index")
}

fn write_keyring_index(service: &str, keys: &[String]) -> Result<()> {
    keyring_entry(service, KEYRING_INDEX_USER)?
        .set_password(&serde_json::to_string(keys)?)
        .context("writing system keyring secret index")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_filters_to_secret_like_keys() {
        let entries = BTreeMap::from([
            ("OPENAI_API_KEY".to_string(), "sk-test".to_string()),
            (
                "OPENAI_BASE_URL".to_string(),
                "https://example.com".to_string(),
            ),
            ("FEISHU_APP_SECRET".to_string(), "secret-value".to_string()),
        ]);

        let redacted = redaction_entries(&entries);

        assert!(redacted.contains_key("OPENAI_API_KEY"));
        assert!(redacted.contains_key("FEISHU_APP_SECRET"));
        assert!(!redacted.contains_key("OPENAI_BASE_URL"));
    }

    #[test]
    fn validates_env_key_names() {
        assert!(validate_secret_key("EXA_API_KEY").is_ok());
        assert!(validate_secret_key("1BAD").is_err());
        assert!(validate_secret_key("BAD-NAME").is_err());
    }
}
