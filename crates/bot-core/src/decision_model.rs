use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const EMBEDDED_JEV_PROFILE: &str = include_str!("../decision-models/jev.yaml");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionModelProfileConfig {
    pub id: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub timeout_ms: u64,
}

impl DecisionModelProfileConfig {
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        {
            bail!("invalid decision model id `{}`", self.id);
        }
        if self.model.trim().is_empty() {
            bail!("decision model `{}` is missing model", self.id);
        }
        if self.api_key_env.is_empty()
            || !self.api_key_env.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_uppercase() || byte == b'_' || (index > 0 && byte.is_ascii_digit())
            })
        {
            bail!("decision model `{}` has invalid api_key_env", self.id);
        }
        if self.timeout_ms == 0 {
            bail!("decision model `{}` timeout_ms must be > 0", self.id);
        }
        let url = reqwest::Url::parse(&self.base_url)
            .with_context(|| format!("decision model `{}` has invalid base_url", self.id))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("decision model `{}` base_url must be an HTTP(S) URL without credentials, query, or fragment", self.id);
        }
        Ok(())
    }

    pub fn systemone_url(&self) -> Result<reqwest::Url> {
        self.validate()?;
        let mut url = reqwest::Url::parse(&self.base_url)?;
        let path = format!("{}/systemone", url.path().trim_end_matches('/'));
        url.set_path(&path);
        Ok(url)
    }

    pub fn api_key(&self, values: Option<&BTreeMap<String, String>>) -> Result<String> {
        let value = match values {
            Some(values) => values.get(&self.api_key_env).cloned(),
            None => std::env::var(&self.api_key_env).ok(),
        };
        value
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} must be set for decision model `{}`",
                    self.api_key_env,
                    self.id
                )
            })
    }
}

pub fn install_embedded_decision_model_profiles(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("creating decision models dir {}", dir.display()))?;
    let path = dir.join("jev.yaml");
    if !path.exists() {
        fs::write(&path, EMBEDDED_JEV_PROFILE)
            .with_context(|| format!("seeding decision model {}", path.display()))?;
    }
    Ok(())
}

pub fn load_decision_model_profile(dir: &Path, id: &str) -> Result<DecisionModelProfileConfig> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        bail!("invalid decision model id `{id}`");
    }
    let path = dir.join(format!("{id}.yaml"));
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("reading decision model {}", path.display()))?;
    let profile: DecisionModelProfileConfig = serde_yaml::from_str(&raw)
        .with_context(|| format!("parsing decision model {}", path.display()))?;
    profile.validate()?;
    if profile.id != id {
        bail!(
            "decision model file {} has id `{}`, expected `{id}`",
            path.display(),
            profile.id
        );
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_profile_and_custom_endpoint_validate() {
        let dir = tempfile::tempdir().unwrap();
        install_embedded_decision_model_profiles(dir.path()).unwrap();
        let mut profile = load_decision_model_profile(dir.path(), "jev").unwrap();
        let keys = BTreeMap::from([("TYPESAFE_API_KEY".to_string(), "test-secret".to_string())]);
        assert_eq!(profile.api_key(Some(&keys)).unwrap(), "test-secret");
        assert_eq!(
            profile.systemone_url().unwrap().as_str(),
            "https://api.typesafe.ai/v1/systemone"
        );
        profile.base_url = "http://127.0.0.1:8080/custom/v1/".into();
        profile.model = "local-jev".into();
        assert_eq!(
            profile.systemone_url().unwrap().as_str(),
            "http://127.0.0.1:8080/custom/v1/systemone"
        );
        profile.base_url = "https://key:secret@example.com/v1".into();
        assert!(profile.validate().is_err());
        assert!(load_decision_model_profile(dir.path(), "../other").is_err());
    }
}
