//! Shared authentication selection; subscription credentials are never API keys.
use super::ChatGptAuthManager;
use crate::ModelProfileConfig;
use anyhow::Result;
use std::{collections::BTreeMap, path::Path};

#[derive(Clone)]
pub enum ModelCredential {
    ApiKey(String),
    ChatGpt(ChatGptAuthManager),
}
impl ModelCredential {
    /// Compatibility value for legacy API-key-only tools. Subscription tokens
    /// never pass through those tools; the Responses client owns authentication.
    pub fn legacy_api_key(&self) -> String {
        match self {
            Self::ApiKey(key) => key.clone(),
            Self::ChatGpt(_) => String::new(),
        }
    }
}
pub fn resolve_model_credential(
    profile: &ModelProfileConfig,
    data_dir: &Path,
    values: Option<&BTreeMap<String, String>>,
) -> Result<ModelCredential> {
    if profile.provider.as_deref() == Some("chatgpt") {
        profile.validate()?;
        let auth = ChatGptAuthManager::new(data_dir);
        auth.ensure_configured()?;
        Ok(ModelCredential::ChatGpt(auth))
    } else {
        let key = match values {
            Some(values) => crate::api_key_from_values(profile, values)?,
            None => crate::api_key_from_env(profile)?,
        };
        Ok(ModelCredential::ApiKey(key))
    }
}
pub fn model_auth_key_from_env(profile: &ModelProfileConfig) -> Result<String> {
    let dir = std::env::var_os("REMI_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".remi-cat".into());
    Ok(resolve_model_credential(profile, &dir, None)?.legacy_api_key())
}
pub fn model_auth_key_from_values(
    profile: &ModelProfileConfig,
    values: &BTreeMap<String, String>,
) -> Result<String> {
    let dir = std::env::var_os("REMI_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".remi-cat".into());
    Ok(resolve_model_credential(profile, &dir, Some(values))?.legacy_api_key())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_luna_review_tracks_chatgpt_switches_and_respects_overrides() {
        let dir = tempfile::tempdir().unwrap();
        super::super::auth::test_auth(dir.path());
        let models = dir.path().join("models");
        std::fs::create_dir(&models).unwrap();
        let mut main: ModelProfileConfig = serde_yaml::from_str("id: main\nname: Main\nmodel: gpt-6.1-sol\nprovider: chatgpt\ncontext_tokens: 32768\nmax_output_tokens: 4096\nsupports_images: false\noverflow_bytes: 32768\ncontext_compaction: hard\n").unwrap();
        for (id, model, provider) in [
            ("main", "gpt-6.1-sol", Some("chatgpt")),
            ("second", "gpt-6-astra", Some("chatgpt")),
            ("override", "gpt-6-sol", Some("chatgpt")),
            ("api", "test-api-model", None),
        ] {
            main.id = id.into();
            main.model = model.into();
            main.provider = provider.map(str::to_owned);
            std::fs::write(
                models.join(format!("{id}.yaml")),
                serde_yaml::to_string(&main).unwrap(),
            )
            .unwrap();
        }
        let keys = BTreeMap::from([("OPENAI_API_KEY".into(), "test-only".into())]);
        let build = |id| {
            crate::CatBotBuilder::from_model_source_with_api_keys(
                dir.path(),
                dir.path(),
                Some(id),
                &keys,
            )
            .unwrap()
        };
        let bot = build("main").build().unwrap();
        assert_eq!(bot.approval_model_for(None), Some("gpt-6-luna"));
        assert_eq!(bot.approval_model_for(Some("second")), Some("gpt-6-luna"));
        let bot = build("api").build().unwrap();
        assert_eq!(bot.approval_model_for(None), Some("test-api-model"));
        assert_eq!(bot.approval_model_for(Some("second")), Some("gpt-6-luna"));
        let bot = build("main")
            .approval_model_profile("override")
            .build()
            .unwrap();
        assert_eq!(bot.approval_model_for(None), Some("gpt-6-sol"));
        assert_eq!(bot.approval_model_for(Some("second")), Some("gpt-6-sol"));
        let decision = crate::DecisionModelProfileConfig {
            id: "test-jev".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test-jev".into(),
            api_key_env: "TEST_ONLY".into(),
            timeout_ms: 1000,
        };
        let bot = build("main")
            .approval_model_profile("missing")
            .jev_approval(decision, "test-only".into(), 9900)
            .build()
            .unwrap();
        assert_eq!(bot.approval_model_for(None), Some("test-jev"));
        assert_eq!(bot.approval_model_for(Some("second")), Some("test-jev"));
    }
    #[test]
    fn embedded_runtime_uses_its_own_auth_root_without_an_api_key() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        super::super::auth::test_auth(dir.path());
        let profile: ModelProfileConfig = serde_yaml::from_str("id: subscription\nname: Subscription\nmodel: test-model\nprovider: chatgpt\ncontext_tokens: 32768\nmax_output_tokens: 4096\nsupports_images: true\noverflow_bytes: 32768\ncontext_compaction: hard\n").unwrap();
        let keys = BTreeMap::new();
        assert!(matches!(
            resolve_model_credential(&profile, dir.path(), Some(&keys)).unwrap(),
            ModelCredential::ChatGpt(_)
        ));
        assert!(resolve_model_credential(&profile, other.path(), Some(&keys)).is_err());
        std::fs::create_dir(dir.path().join("models")).unwrap();
        std::fs::write(
            dir.path().join("models/subscription.yaml"),
            serde_yaml::to_string(&profile).unwrap(),
        )
        .unwrap();
        let bot = crate::CatBotBuilder::from_model_source_with_api_keys(
            dir.path(),
            dir.path(),
            Some("subscription"),
            &keys,
        )
        .unwrap()
        .build()
        .unwrap();
        bot.ensure_model_authenticated(&profile).unwrap();
    }
}
