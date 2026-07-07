//! Context compression layer.
//!
//! `CompressingLayer<M>` wraps any `Agent<Request=LoopInput>` and automatically
//! summarises long conversation histories before forwarding the request to the
//! inner agent, keeping the context window under control.
//!
//! ## How it works
//!
//! On each `LoopInput::Start { history, .. }`:
//! 1. Estimate total chars in history.
//! 2. If it exceeds `threshold_chars`, take the oldest messages (everything
//!    before `keep_recent` from the end), pass them to the model as a
//!    summarisation request, and replace them with a single
//!    `Message::user("[Summary: ...]")`.
//! 3. Forward the now-shorter history to the inner agent.

use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_core::agent::Agent;
use remi_core::error::AgentError;
use remi_core::model::ChatModel;
use remi_core::types::{
    AgentEvent, ChatRequest, ChatResponseChunk, Content, LoopInput, Message, Role,
};

use crate::events::DeepAgentEvent;

// ── Defaults ──────────────────────────────────────────────────────────────────

const DEFAULT_THRESHOLD_CHARS: usize = 80_000; // ≈ 20k tokens
const DEFAULT_KEEP_RECENT: usize = 10; // always preserve last 10 messages

// ── CompressingLayer ──────────────────────────────────────────────────────────

/// Layer that compresses conversation history before forwarding to the inner agent.
pub struct CompressingLayer<M> {
    /// The model used for summarisation (can be the same as the main model,
    /// or a cheaper/faster variant).
    pub model: M,
    /// Model identifier sent in the summarisation `ChatRequest`.
    pub model_name: String,
    /// Approximate character threshold — compression is triggered when the total
    /// chars in `history` exceed this value.
    pub threshold_chars: usize,
    /// How many messages to keep verbatim from the end of history.
    pub keep_recent: usize,
}

impl<M: ChatModel + Clone> CompressingLayer<M> {
    pub fn new(model: M, model_name: impl Into<String>) -> Self {
        Self {
            model,
            model_name: model_name.into(),
            threshold_chars: DEFAULT_THRESHOLD_CHARS,
            keep_recent: DEFAULT_KEEP_RECENT,
        }
    }

    pub fn threshold(mut self, chars: usize) -> Self {
        self.threshold_chars = chars;
        self
    }

    pub fn keep_recent(mut self, n: usize) -> Self {
        self.keep_recent = n;
        self
    }
}

// ── CompressingAgent ──────────────────────────────────────────────────────────

pub struct CompressingAgent<A, M> {
    inner: A,
    model: M,
    model_name: String,
    threshold_chars: usize,
    keep_recent: usize,
}

impl<A, M> CompressingAgent<A, M>
where
    M: ChatModel + Clone,
{
    async fn maybe_compress(&self, history: Vec<Message>) -> Result<Vec<Message>, AgentError> {
        let total_chars: usize = history.iter().map(|m| m.content.text_content().len()).sum();
        if total_chars <= self.threshold_chars || history.len() <= self.keep_recent {
            return Ok(history);
        }

        // Split: old part to summarise, recent part to keep verbatim
        let keep_n = self.keep_recent.min(history.len());
        let split_at = history.len() - keep_n;

        // Never summarise system messages — preserve them at the front
        let (sys_msgs, rest): (Vec<_>, Vec<_>) = history[..split_at]
            .iter()
            .partition(|m| m.role == Role::System);
        let to_summarise: Vec<_> = rest;
        let recent = history[split_at..].to_vec();

        if to_summarise.is_empty() {
            return Ok(history);
        }

        // Build a summarisation prompt
        let history_text = to_summarise
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::Tool => "Tool",
                    Role::System => "System",
                };
                format!("[{role}]: {}", m.content.text_content())
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let req = ChatRequest {
            model: self.model_name.clone(),
            messages: vec![
                Message::system(
                    "You are a context summariser. Produce a concise but complete \
                     summary of the conversation history below, preserving key decisions, \
                     tool results, and any facts the assistant will need going forward.",
                ),
                Message::user(format!(
                    "Summarise this conversation history:\n\n{history_text}"
                )),
            ],
            tools: None,
            temperature: Some(0.3),
            max_tokens: Some(800),
            stream: true,
            stream_options: None,
            metadata: None,
            rate_limit_retry: None,
            extra_body: serde_json::Map::new(),
        };

        let stream = self.model.chat(req).await?;
        let mut stream = std::pin::pin!(stream);
        let mut summary = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                ChatResponseChunk::Delta { content, .. } => summary.push_str(&content),
                ChatResponseChunk::Done => break,
                _ => {}
            }
        }

        // Reconstruct: system msgs → summary message → recent msgs
        let mut compressed: Vec<Message> = sys_msgs.into_iter().cloned().collect();
        compressed.push(Message::user(format!(
            "[Context compressed — previous conversation summary]\n{summary}"
        )));
        compressed.extend(recent);
        Ok(compressed)
    }
}

impl<A, M> Agent for CompressingAgent<A, M>
where
    A: Agent<Request = LoopInput, Response = DeepAgentEvent, Error = AgentError>,
    M: ChatModel + Clone,
{
    type Request = LoopInput;
    type Response = DeepAgentEvent;
    type Error = AgentError;

    async fn chat(
        &self,
        input: LoopInput,
    ) -> Result<impl Stream<Item = DeepAgentEvent>, AgentError> {
        let input = match input {
            LoopInput::Start {
                content,
                history,
                extra_tools,
                model,
                temperature,
                max_tokens,
                metadata,
                message_metadata,
                user_name,
                user_state,
            } => {
                let history = self.maybe_compress(history).await?;
                LoopInput::Start {
                    content,
                    history,
                    extra_tools,
                    model,
                    temperature,
                    max_tokens,
                    metadata,
                    message_metadata,
                    user_name,
                    user_state,
                }
            }
            other => other,
        };

        let stream = self.inner.chat(input).await?;
        Ok(stream)
    }
}

// ── Layer impl ────────────────────────────────────────────────────────────────

impl<A, M> remi_core::agent::Layer<A> for CompressingLayer<M>
where
    A: Agent<Request = LoopInput, Response = DeepAgentEvent, Error = AgentError>,
    M: ChatModel + Clone,
{
    type Output = CompressingAgent<A, M>;
    fn layer(self, inner: A) -> Self::Output {
        CompressingAgent {
            inner,
            model: self.model,
            model_name: self.model_name,
            threshold_chars: self.threshold_chars,
            keep_recent: self.keep_recent,
        }
    }
}
