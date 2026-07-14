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
Return a concise, information-dense summary with exactly these non-empty headings:\n\
## Goal and latest user intent\n\
## Constraints and prohibitions\n\
## Confirmed facts and decisions\n\
## Completed work with evidence\n\
## Current state\n\
## Pending work and next action\n\
## Failures and uncertainties\n\
## Exact references\n\
For tool activity retain the tool name, concise arguments, success status, duration, key result, \
errors, paths, IDs, and exact values. Output only the summary.";

// ── LlmCompressor ────────────────────────────────────────────────────────────

pub struct LlmCompressor {
    inner: AgentLoop<OpenAIClient<ReqwestTransport>>,
}

impl LlmCompressor {
    pub fn new(
        api_key: String,
        base_url: Option<String>,
        model: String,
        max_output_tokens: u32,
        extra_options: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        let mut oai = OpenAIClient::new(api_key).with_model(model);
        if let Some(url) = base_url {
            oai = oai.with_base_url(url);
        }
        let agent_config = AgentConfig::default().with_max_tokens(max_output_tokens);
        let mut builder = AgentBuilder::new()
            .model(oai)
            .config(agent_config)
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
    pub async fn compress(&self, messages: &[Message]) -> Result<String, AgentError> {
        if messages.is_empty() {
            return Ok(String::new());
        }

        // Tool results are part of the durable evidence. Bound only the input
        // presented to the compressor; the ledger always retains full content.
        let text: String = messages
            .iter()
            .map(|m| {
                let role = format!("{:?}", m.role);
                let raw = m.content.text_content();
                let body = truncate_tool_body(&role, &raw, 12_000);
                if body.is_empty() {
                    String::new()
                } else {
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
                }
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");

        if text.is_empty() {
            tracing::warn!(
                "LlmCompressor: all {} messages empty, nothing to compress",
                messages.len(),
            );
            return Ok(String::new());
        }

        let input = LoopInput::start(text);
        let stream = self
            .inner
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
        validate_summary(&result)?;
        Ok(result)
    }
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

fn validate_summary(summary: &str) -> Result<(), AgentError> {
    const HEADINGS: [&str; 8] = [
        "## Goal and latest user intent",
        "## Constraints and prohibitions",
        "## Confirmed facts and decisions",
        "## Completed work with evidence",
        "## Current state",
        "## Pending work and next action",
        "## Failures and uncertainties",
        "## Exact references",
    ];
    if summary.chars().count() > 40_000 || HEADINGS.iter().any(|heading| !summary.contains(heading))
    {
        return Err(AgentError::other(
            "LlmCompressor: summary failed structure/length validation",
        ));
    }
    Ok(())
}
