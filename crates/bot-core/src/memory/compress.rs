//! LLM-based memory compressor.
//!
//! `LlmCompressor` spins up a single-turn `AgentLoop` (same API credentials,
//! no tools) and uses it to summarise a slice of `Message`s into a compact
//! text block suitable for long- or mid-term memory storage.

use futures::StreamExt;

use remi_agentloop::agent_loop::AgentLoop;
use remi_agentloop::prelude::{
    Agent, AgentBuilder, AgentConfig, AgentError, LoopInput, Message, OpenAIClient,
    ReqwestTransport,
};
use remi_agentloop::types::AgentEvent;

const COMPRESSION_SYSTEM: &str = "\
You are a memory compression assistant. Preserve the conversation's primary language. \
Never translate code, commands, paths, identifiers, exact values, or error text. \
Resolve knowledge updates chronologically: label older values as superseded and state only the \
latest confirmed value in Current state. Never collapse distinct old and new exact values into a range. \
Prefer completeness over brevity. Preserve concrete facts even when mentioned incidentally. Represent \
facts as losslessly as possible with their entity, attribute, exact value, timestamp, provenance, and \
state transition when present. Preserve dependencies required to reproduce a stated or implied result, \
not only the result itself. Do not replace an exact fact with a vague category or omit it merely because \
it appears unrelated to the latest goal. \
Return a concise, information-dense summary. Choose whatever structure best preserves the source. \
Retain the latest user intent, constraints, confirmed facts and decisions, completed work and evidence, \
current state, pending work, failures and uncertainties, and exact references whenever present. \
For tool activity retain the tool name, concise arguments, success status, duration, key result, \
errors, paths, IDs, and exact values. Output only the summary.";

// ── LlmCompressor ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct LlmCompressor {
    api_key: String,
    base_url: Option<String>,
    model: String,
    context_tokens: u32,
    max_output_tokens: u32,
    extra_options: serde_json::Map<String, serde_json::Value>,
}

impl LlmCompressor {
    pub fn new(
        api_key: String,
        base_url: Option<String>,
        model: String,
        context_tokens: u32,
        max_output_tokens: u32,
        extra_options: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        Self {
            api_key,
            base_url,
            model,
            context_tokens,
            max_output_tokens,
            extra_options,
        }
    }

    fn build_loop(&self, output_tokens: u32) -> AgentLoop<OpenAIClient<ReqwestTransport>> {
        let mut oai = OpenAIClient::new(self.api_key.clone()).with_model(self.model.clone());
        if let Some(url) = self.base_url.clone() {
            oai = oai.with_base_url(url);
        }
        let agent_config = AgentConfig::default().with_max_tokens(output_tokens);
        let mut builder = AgentBuilder::new()
            .model(oai)
            .config(agent_config)
            .system(COMPRESSION_SYSTEM)
            .max_turns(1);
        if !self.extra_options.is_empty() {
            builder = builder.extra_options(self.extra_options.clone());
        }
        builder.build_loop()
    }

    fn output_budget(&self, source_tokens: u32) -> u32 {
        source_tokens
            .div_ceil(4)
            .max(1_024)
            .min(8_192)
            .min((self.context_tokens / 10).max(1))
            .min(self.max_output_tokens.max(1))
    }

    pub fn input_fits(&self, messages: &[Message]) -> bool {
        let text = compression_input_text(messages);
        let source = crate::estimate_model_input_tokens(&text);
        let output = self.output_budget(source);
        let safety = self.context_tokens.saturating_mul(5).div_ceil(100);
        let system = crate::estimate_model_input_tokens(COMPRESSION_SYSTEM);
        system
            .saturating_add(source)
            .saturating_add(output)
            .saturating_add(safety)
            .saturating_add(512)
            <= self.context_tokens
    }

    /// Compress a slice of messages into a summary string.
    ///
    pub async fn compress(&self, messages: &[Message]) -> Result<String, AgentError> {
        if messages.is_empty() {
            return Ok(String::new());
        }

        // Tool results are part of the durable evidence. Bound only the input
        // presented to the compressor; the ledger always retains full content.
        let text = compression_input_text(messages);

        if text.is_empty() {
            tracing::warn!(
                "LlmCompressor: all {} messages empty, nothing to compress",
                messages.len(),
            );
            return Ok(String::new());
        }

        let source_tokens = crate::estimate_model_input_tokens(&text);
        if !self.input_fits(messages) {
            return Err(AgentError::other(format!(
                "LlmCompressor: compression input does not fit model context (estimated {source_tokens} source tokens)"
            )));
        }
        let input = LoopInput::start(text);
        let inner = self.build_loop(self.output_budget(source_tokens));
        let stream = inner
            .chat(bot_runtime_core::chat_ctx_from_input(&input, None), input)
            .await?;
        let mut stream = std::pin::pin!(stream);

        let mut result = String::new();
        while let Some(ev) = stream.next().await {
            if let AgentEvent::TextDelta(delta) = ev {
                result.push_str(&delta);
            }
        }

        if result.trim().is_empty() {
            return Err(AgentError::Io(
                "LlmCompressor: model returned empty summary".to_string(),
            ));
        }
        Ok(result)
    }
}

fn compression_input_text(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| {
            let role = format!("{:?}", m.role);
            let raw = m.content.text_content();
            let body = truncate_tool_body(&role, &raw, 12_000);
            if body.is_empty() {
                return String::new();
            }
            let calls = m.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|call| {
                        format!(
                            "{}({}) id={}",
                            call.function.name, call.function.arguments, call.id
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            });
            format!(
                "[{role}] id={}{}\n{body}",
                m.id,
                calls
                    .map(|v| format!(" tool_calls={v}"))
                    .unwrap_or_default()
            )
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn truncate_tool_body(role: &str, text: &str, max_chars: usize) -> String {
    if role != "Tool" || text.chars().count() <= max_chars {
        return text.to_string();
    }
    let half = max_chars / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(half)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n...[tool result elided for summary input only]...\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_prompt_allows_free_form_output() {
        assert!(COMPRESSION_SYSTEM.contains("whatever structure"));
        assert!(!COMPRESSION_SYSTEM.contains("exactly these non-empty headings"));
    }

    #[test]
    fn compression_output_budget_is_dynamic_and_bounded() {
        let compressor = LlmCompressor::new(
            "key".to_string(),
            None,
            "model".to_string(),
            128_000,
            32_000,
            serde_json::Map::new(),
        );
        assert_eq!(compressor.output_budget(100), 1_024);
        assert_eq!(compressor.output_budget(20_000), 5_000);
        assert_eq!(compressor.output_budget(100_000), 8_192);
    }

    #[test]
    fn compressor_preflight_rejects_source_that_cannot_fit() {
        let compressor = LlmCompressor::new(
            "key".to_string(),
            None,
            "model".to_string(),
            4_096,
            2_048,
            serde_json::Map::new(),
        );
        assert!(!compressor.input_fits(&[Message::user("x".repeat(40_000))]));
    }
}
