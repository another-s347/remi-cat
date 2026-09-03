use futures::Stream;
use remi_agentloop::agent_loop::AgentLoop;
use remi_agentloop::prelude::{
    Agent, AgentBuilder, AgentConfig, AgentError, ChatCtx, ChatResponseChunk, Content, ContentPart,
    Message, MiMoClient, ModelRequest, OpenAIClient, ReqwestTransport,
};
use std::future::Future;
use std::pin::Pin;

use crate::{context_budget_tokens, ModelProfileConfig, ModelProfileRegistry, SharedRedactor};

use super::{sentry_tracer, AgentTracingOptions, DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT};

#[derive(Clone)]
pub(super) enum ProviderClient {
    OpenAI {
        client: OpenAIClient<ReqwestTransport>,
        supports_multimodal: bool,
    },
    MiMo {
        client: MiMoClient<ReqwestTransport>,
        supports_multimodal: bool,
    },
}

impl Agent for ProviderClient {
    type Request = ModelRequest;
    type Response = ChatResponseChunk;
    type Error = AgentError;

    #[allow(refining_impl_trait)]
    fn chat(
        &self,
        ctx: ChatCtx,
        mut req: ModelRequest,
    ) -> impl Future<Output = Result<Pin<Box<dyn Stream<Item = ChatResponseChunk> + '_>>, AgentError>> + '_
    {
        async move {
            let supports_multimodal = match self {
                Self::OpenAI {
                    supports_multimodal,
                    ..
                }
                | Self::MiMo {
                    supports_multimodal,
                    ..
                } => *supports_multimodal,
            };
            if !supports_multimodal {
                let lowered_messages = lower_messages_for_text_model(&mut req.messages);
                if lowered_messages > 0 {
                    tracing::info!(
                        model = %req.model,
                        lowered_messages,
                        "lowered multimodal model input to text"
                    );
                }
            }
            match self {
                Self::OpenAI { client, .. } => client.chat(ctx, req).await.map(|stream| {
                    Box::pin(stream) as Pin<Box<dyn Stream<Item = ChatResponseChunk> + '_>>
                }),
                Self::MiMo { client, .. } => client.chat(ctx, req).await.map(|stream| {
                    Box::pin(stream) as Pin<Box<dyn Stream<Item = ChatResponseChunk> + '_>>
                }),
            }
        }
    }
}

fn lower_messages_for_text_model(messages: &mut [Message]) -> usize {
    let mut lowered = 0;
    for message in messages {
        if message.content.is_multimodal() {
            message.content = lower_content_to_text(&message.content);
            lowered += 1;
        }
    }
    lowered
}

fn lower_content_to_text(content: &Content) -> Content {
    let Content::Parts(parts) = content else {
        return content.clone();
    };
    Content::text(
        parts
            .iter()
            .map(lower_content_part_to_text)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn lower_content_part_to_text(part: &ContentPart) -> String {
    match part {
        ContentPart::Text { text } => text.clone(),
        ContentPart::ImageUrl { image_url } => {
            let url = image_url.url.trim();
            if let Some(header) = url
                .strip_prefix("data:")
                .and_then(|value| value.split(',').next())
            {
                format!("[image: embedded {header}; data omitted]")
            } else if url.is_empty() {
                "[image]".to_string()
            } else {
                format!("[image: {url}]")
            }
        }
        ContentPart::ImageBase64 { media_type, .. } => {
            let media_type = media_type.trim();
            if media_type.is_empty() {
                "[image: embedded data omitted]".to_string()
            } else {
                format!("[image: {media_type}; embedded data omitted]")
            }
        }
        ContentPart::Audio { input_audio } => {
            let format = input_audio.format.trim();
            if format.is_empty() {
                "[audio: embedded data omitted]".to_string()
            } else {
                format!("[audio: format={format}; embedded data omitted]")
            }
        }
        ContentPart::File {
            file_id,
            filename,
            media_type,
            data,
        } => {
            let mut attributes = Vec::new();
            if let Some(filename) = filename.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                attributes.push(format!("filename={filename}"));
            }
            if let Some(media_type) = media_type
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
            {
                attributes.push(format!("media_type={media_type}"));
            }
            if let Some(file_id) = file_id.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                attributes.push(format!("file_id={file_id}"));
            }
            if data.is_some() {
                attributes.push("embedded data omitted".to_string());
            }
            if attributes.is_empty() {
                "[file]".to_string()
            } else {
                format!("[file: {}]", attributes.join("; "))
            }
        }
    }
}

pub(super) type InnerAgent = AgentLoop<ProviderClient>;

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
    tracing: AgentTracingOptions,
    agent_name: &str,
    redactor: &SharedRedactor,
) -> InnerAgent {
    let model = build_provider_client(api_key, profile, profile.base_url.clone());
    let mut builder = AgentBuilder::new()
        .model(model)
        .config(
            AgentConfig::default()
                .with_model(profile.model.clone())
                .with_max_tokens(profile.max_output_tokens),
        )
        .system(system_prompt)
        .max_turns(max_turns.unwrap_or(usize::MAX));
    if !extra_options.is_empty() {
        builder = builder.extra_options(extra_options);
    }
    if let Some(tracer) = sentry_tracer(tracing, agent_name, profile, redactor) {
        builder = builder.tracer(tracer);
    }
    builder.build_loop()
}

pub(super) fn build_provider_client(
    api_key: &str,
    profile: &ModelProfileConfig,
    base_url: Option<String>,
) -> ProviderClient {
    if profile.provider.as_deref() == Some("mimo") {
        let mut client = MiMoClient::new(api_key.to_string()).with_model(profile.model.clone());
        if let Some(url) = base_url {
            client = client.with_base_url(url);
        }
        ProviderClient::MiMo {
            client,
            supports_multimodal: profile.supports_images,
        }
    } else {
        let mut client = OpenAIClient::new(api_key.to_string()).with_model(profile.model.clone());
        if let Some(url) = base_url {
            client = client.with_base_url(url);
        }
        ProviderClient::OpenAI {
            client,
            supports_multimodal: profile.supports_images,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use remi_agentloop::types::AudioDetail;

    fn profile(provider: Option<&str>) -> ModelProfileConfig {
        ModelProfileConfig {
            id: "test".to_string(),
            name: "Test".to_string(),
            model: "test-model".to_string(),
            base_url: Some("https://example.invalid/v1".to_string()),
            thinking: None,
            reasoning_effort: None,
            max_output_tokens: 1024,
            context_tokens: 8192,
            supports_images: true,
            legacy_short_term_tokens: None,
            overflow_bytes: 16_384,
            context_compaction: crate::ContextCompactionMode::Hard,
            description: None,
            provider: provider.map(str::to_string),
            extra_options: serde_json::Map::new(),
        }
    }

    #[test]
    fn mimo_provider_uses_mimo_client() {
        assert!(matches!(
            build_provider_client("test-key", &profile(Some("mimo")), None),
            ProviderClient::MiMo { .. }
        ));
    }

    #[test]
    fn other_providers_keep_using_openai_compatible_client() {
        assert!(matches!(
            build_provider_client("test-key", &profile(Some("openai")), None),
            ProviderClient::OpenAI { .. }
        ));
        assert!(matches!(
            build_provider_client("test-key", &profile(None), None),
            ProviderClient::OpenAI { .. }
        ));
    }

    #[test]
    fn text_model_lowering_preserves_order_and_omits_embedded_data() {
        let mut messages = vec![Message::user_content(Content::parts(vec![
            ContentPart::text("inspect"),
            ContentPart::image_url("https://example.test/photo.png"),
            ContentPart::image_base64("image/png", "SECRET_IMAGE_DATA"),
            ContentPart::Audio {
                input_audio: AudioDetail {
                    data: "SECRET_AUDIO_DATA".to_string(),
                    format: "wav".to_string(),
                },
            },
            ContentPart::File {
                file_id: Some("file-1".to_string()),
                filename: Some("notes.pdf".to_string()),
                media_type: Some("application/pdf".to_string()),
                data: Some("SECRET_FILE_DATA".to_string()),
            },
        ]))];

        assert_eq!(lower_messages_for_text_model(&mut messages), 1);
        let text = messages[0].content.text_content();
        assert_eq!(
            text,
            "inspect\n[image: https://example.test/photo.png]\n[image: image/png; embedded data omitted]\n[audio: format=wav; embedded data omitted]\n[file: filename=notes.pdf; media_type=application/pdf; file_id=file-1; embedded data omitted]"
        );
        assert!(!text.contains("SECRET_"));
        assert!(!messages[0].content.is_multimodal());
    }

    #[test]
    fn text_model_lowering_redacts_data_urls_but_keeps_plain_text_unchanged() {
        let mut messages = vec![
            Message::user("plain"),
            Message::user_content(Content::parts(vec![ContentPart::image_url(
                "data:image/jpeg;base64,SECRET_IMAGE_DATA",
            )])),
        ];

        assert_eq!(lower_messages_for_text_model(&mut messages), 1);
        assert_eq!(messages[0].content.text_content(), "plain");
        assert_eq!(
            messages[1].content.text_content(),
            "[image: embedded image/jpeg;base64; data omitted]"
        );
    }
}
