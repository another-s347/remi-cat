//! LLM-based memory compressor.
//!
//! `LlmCompressor` spins up a single-turn `AgentLoop` (same API credentials,
//! no tools) and uses it to summarise a slice of `Message`s into a compact
//! text block suitable for long- or mid-term memory storage.

use futures::StreamExt;

use remi_agentloop::agent_loop::AgentLoop;
use remi_agentloop::prelude::{
    Agent, AgentBuilder, AgentError, LoopInput, Message, OpenAIClient, ReqwestTransport,
};
use remi_agentloop::types::AgentEvent;

const COMPRESSION_SYSTEM: &str = "\
You are a memory compression assistant. \
Compress the following conversation into a concise, information-dense summary in Chinese. \
Preserve all key facts, decisions, plans, and outcomes. \
Output only the summary text — no preamble, no commentary.";

// ── LlmCompressor ────────────────────────────────────────────────────────────

pub struct LlmCompressor {
    inner: AgentLoop<OpenAIClient<ReqwestTransport>>,
}

impl LlmCompressor {
    pub fn new(
        api_key: String,
        base_url: Option<String>,
        model: String,
        extra_options: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        let mut oai = OpenAIClient::new(api_key).with_model(model);
        if let Some(url) = base_url {
            oai = oai.with_base_url(url);
        }
        let mut builder = AgentBuilder::new()
            .model(oai)
            .system(COMPRESSION_SYSTEM)
            .max_turns(1);
        if !extra_options.is_empty() {
            builder = builder.extra_options(extra_options);
        }
        let inner = builder.build_loop();
        Self { inner }
    }

    /// Compress a slice of messages into a summary string.
    ///
    /// Tool-response messages are excluded before compression — they are
    /// typically large, noisy, and not useful in a long-term summary.
    pub async fn compress(&self, messages: &[Message]) -> Result<String, AgentError> {
        if messages.is_empty() {
            return Ok(String::new());
        }

        // Format messages as plain text, skipping Tool responses.
        let text: String = messages
            .iter()
            .filter(|m| format!("{:?}", m.role) != "Tool")
            .map(|m| {
                let role = format!("{:?}", m.role); // "User" / "Assistant" / "System"
                let body = m.content.text_content();
                if body.is_empty() {
                    String::new()
                } else {
                    format!("[{role}]\n{body}")
                }
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");

        if text.is_empty() {
            return Ok(String::new());
        }

        let input = LoopInput::start(text);
        let stream = self.inner.chat(input).await?;
        let mut stream = std::pin::pin!(stream);

        let mut result = String::new();
        while let Some(ev) = stream.next().await {
            if let AgentEvent::TextDelta(delta) = ev {
                result.push_str(&delta);
            }
        }

        if result.is_empty() {
            return Err(AgentError::Io(
                "LlmCompressor: model returned empty summary".to_string(),
            ));
        }
        Ok(result)
    }
}
