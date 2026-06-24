use remi_agentloop::agent_loop::AgentLoop;
use remi_agentloop::prelude::{AgentBuilder, AgentConfig, OpenAIClient, ReqwestTransport};

use crate::{context_budget_tokens, ModelProfileConfig, ModelProfileRegistry};

use super::DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT;

pub(super) type InnerAgent = AgentLoop<OpenAIClient<ReqwestTransport>>;

#[derive(Debug, Clone)]
pub struct EffectiveModelProfile {
    pub profile: ModelProfileConfig,
    pub source: EffectiveModelSource,
    pub invalid_session_model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveModelSource {
    Session,
    Default,
}

pub(super) fn resolve_effective_model_profile(
    default_profile: &ModelProfileConfig,
    registry: &ModelProfileRegistry,
    session_model_profile_id: Option<&str>,
) -> EffectiveModelProfile {
    if let Some(id) = session_model_profile_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if let Some(profile) = registry.get(id) {
            return EffectiveModelProfile {
                profile: profile.clone(),
                source: EffectiveModelSource::Session,
                invalid_session_model: None,
            };
        }
        return EffectiveModelProfile {
            profile: default_profile.clone(),
            source: EffectiveModelSource::Default,
            invalid_session_model: Some(id.to_string()),
        };
    }
    EffectiveModelProfile {
        profile: default_profile.clone(),
        source: EffectiveModelSource::Default,
        invalid_session_model: None,
    }
}

pub(super) fn build_inner_agent(
    api_key: &str,
    profile: &ModelProfileConfig,
    system_prompt: String,
    max_turns: Option<usize>,
    extra_options: serde_json::Map<String, serde_json::Value>,
) -> InnerAgent {
    let mut model = OpenAIClient::new(api_key.to_string()).with_model(profile.model.clone());
    if let Some(url) = profile.base_url.clone() {
        model = model.with_base_url(url);
    }
    let mut builder = AgentBuilder::new()
        .model(model)
        .config(AgentConfig::default().with_max_tokens(profile.max_output_tokens))
        .system(system_prompt)
        .max_turns(max_turns.unwrap_or(usize::MAX));
    if !extra_options.is_empty() {
        builder = builder.extra_options(extra_options);
    }
    builder.build_loop()
}

pub(super) fn auto_compress_context_percent() -> anyhow::Result<usize> {
    let Some(raw) = std::env::var("REMI_AUTO_COMPRESS_CONTEXT_PERCENT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT);
    };
    let percent = raw.parse::<usize>().map_err(|err| {
        anyhow::anyhow!("invalid REMI_AUTO_COMPRESS_CONTEXT_PERCENT `{raw}`: {err}")
    })?;
    if !(1..=100).contains(&percent) {
        anyhow::bail!("REMI_AUTO_COMPRESS_CONTEXT_PERCENT must be between 1 and 100");
    }
    Ok(percent)
}

pub(super) fn tool_output_overflow_bytes_from_env() -> anyhow::Result<Option<usize>> {
    let Some((key, raw)) = ["REMI_TOOL_OUTPUT_OVERFLOW_BYTES", "REMI_OVERFLOW_BYTES"]
        .into_iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| (key, value))
        })
    else {
        return Ok(None);
    };
    let bytes = raw
        .parse::<usize>()
        .map_err(|err| anyhow::anyhow!("invalid {key} `{raw}`: {err}"))?;
    if bytes == 0 {
        anyhow::bail!("{key} must be greater than 0");
    }
    Ok(Some(bytes))
}

pub(super) fn context_percent_tokens(profile: &ModelProfileConfig, percent: usize) -> usize {
    context_budget_tokens(profile.context_tokens, percent)
}
