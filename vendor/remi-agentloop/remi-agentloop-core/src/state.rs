//! Pure, stateless step function and associated types.
//!
//! The agent loop is decomposed into a single primitive:
//!
//! ```ignore
//! step(state, action, &model) -> Stream<StepEvent>
//! ```
//!
//! Each call to [`step`] makes exactly **one** model request. The stream
//! yields real-time deltas and terminates with a *terminal event* that
//! carries the updated [`AgentState`]:
//!
//! - [`StepEvent::Done`] — model responded with text only.
//! - [`StepEvent::NeedToolExecution`] — model requested tool calls; caller
//!   executes tools externally and feeds results back via [`Action::ToolResults`].
//! - [`StepEvent::Error`] — an error occurred.
//!
//! Tools are **never** called inside `step()`. The caller (e.g. [`BuiltAgent`](crate::builder::BuiltAgent))
//! is responsible for tool execution, interrupt handling, and turn counting.
//!
//! # Using `step` for external tool execution
//!
//! ```ignore
//! use remi_agentloop_core::prelude::*;
//! use futures::StreamExt;
//!
//! let model = my_model();
//! let mut state = AgentState::new(StepConfig::new("gpt-4o"))
//!     .with_system_prompt("You are a calculator.")
//!     .with_messages(vec![Message::user("What is 3+4?")])
//!     .with_tool_definitions(tool_defs);
//!
//! let mut stream = step(state.clone(), Action::ToolResults(vec![]), &model);
//! while let Some(event) = stream.next().await {
//!     match event {
//!         StepEvent::Done { state: new_state, .. } => { state = new_state; break; }
//!         StepEvent::NeedToolExecution { state: new_state, tool_calls, .. } => {
//!             // execute tools externally…
//!             state = new_state;
//!             let outcomes = execute_externally(&tool_calls).await;
//!             let mut stream2 = step(state.clone(), Action::ToolResults(outcomes), &model);
//!             // … drain stream2
//!         }
//!         _ => {}
//!     }
//! }
//! ```

use async_stream::stream;
use futures::{Stream, StreamExt};

use crate::config::RateLimitRetryPolicy;
use crate::error::AgentError;
use crate::model::ChatModel;
use crate::tool::ToolDefinition;
use crate::types::{
    ChatRequest, ChatResponseChunk, Content, FunctionCall, Message, ParsedToolCall, RunId,
    StreamOptions, ThreadId, ToolCallMessage, ToolCallOutcome,
};

// ── AgentState ────────────────────────────────────────────────────────────────

/// Fully serialisable snapshot of the entire agent execution state.
///
/// `AgentState` is the single source of truth for one run.  It can be:
/// - persisted to a [`CheckpointStore`](crate::checkpoint::CheckpointStore) for crash recovery
/// - serialised to JSON and transferred across processes or WASM boundaries
/// - inspected between steps for debugging or logging
///
/// Create a minimal state with [`AgentState::new`], then use the builder
/// helpers to set optional fields.
///
/// # Example
///
/// ```ignore
/// use remi_agentloop_core::prelude::*;
///
/// let state = AgentState::new(StepConfig::new("gpt-4o"))
///     .with_system_prompt("You are helpful.")
///     .with_messages(vec![Message::user("hello")]);
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentState {
    /// Conversation messages (system, user, assistant, tool results).
    pub messages: Vec<Message>,

    /// System prompt prepended when building the chat request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,

    /// Tool definitions advertised to the model.
    #[serde(default)]
    pub tool_definitions: Vec<ToolDefinition>,

    /// Model & request configuration.
    pub config: StepConfig,

    /// Thread identifier (caller-assigned).
    pub thread_id: ThreadId,

    /// Run identifier (caller-assigned).
    pub run_id: RunId,

    /// Current turn counter (incremented by the caller / outer loop).
    pub turn: usize,

    /// Monotonic per-run model invocation sequence used by tracing/span IDs.
    #[serde(default)]
    pub model_call_seq: usize,

    /// Current phase — indicates what action the caller should take next.
    pub phase: AgentPhase,

    /// Opaque user-defined state carried alongside the conversation.
    #[serde(default)]
    pub user_state: serde_json::Value,
}

impl AgentState {
    /// Create a minimal ready state with required config.
    pub fn new(config: StepConfig) -> Self {
        Self {
            messages: Vec::new(),
            system_prompt: None,
            tool_definitions: Vec::new(),
            config,
            thread_id: ThreadId::new(),
            run_id: RunId::new(),
            turn: 0,
            model_call_seq: 0,
            phase: AgentPhase::Ready,
            user_state: serde_json::Value::Null,
        }
    }

    /// Builder helper: set system prompt.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Builder helper: set tool definitions.
    pub fn with_tool_definitions(mut self, defs: Vec<ToolDefinition>) -> Self {
        self.tool_definitions = defs;
        self
    }

    /// Builder helper: set thread id.
    pub fn with_thread_id(mut self, id: ThreadId) -> Self {
        self.thread_id = id;
        self
    }

    /// Builder helper: set run id.
    pub fn with_run_id(mut self, id: RunId) -> Self {
        self.run_id = id;
        self
    }

    /// Builder helper: set initial messages.
    pub fn with_messages(mut self, msgs: Vec<Message>) -> Self {
        self.messages = msgs;
        self
    }

    /// Builder helper: set user state.
    pub fn with_user_state(mut self, state: serde_json::Value) -> Self {
        self.user_state = state;
        self
    }
}

// ── StepConfig ────────────────────────────────────────────────────────────────

/// Configuration for a single step (model call).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StepConfig {
    /// Model name (e.g. `"gpt-4o"`, `"kimi-k2.5"`).
    pub model: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit_retry: Option<RateLimitRetryPolicy>,

    /// Provider-specific extra fields forwarded verbatim to the model request body.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra_body: serde_json::Map<String, serde_json::Value>,
}

impl StepConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            temperature: None,
            max_tokens: None,
            metadata: None,
            rate_limit_retry: None,
            extra_body: serde_json::Map::new(),
        }
    }
}

fn metadata_flag(metadata: &serde_json::Value, key: &str) -> Option<bool> {
    match metadata.get(key)? {
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" | "enabled" => Some(true),
            "false" | "0" | "no" | "off" | "disabled" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn supports_kimi_thinking_control(model: &str) -> bool {
    model.trim().eq_ignore_ascii_case("kimi-k2.5")
}

fn apply_provider_request_overrides(request: &mut ChatRequest) {
    let Some(metadata) = request.metadata.as_ref() else {
        return;
    };
    let Some(thinking_enabled) = metadata_flag(metadata, "thinking_enabled") else {
        return;
    };

    if supports_kimi_thinking_control(&request.model) {
        request.extra_body.insert(
            "thinking".to_string(),
            serde_json::json!({
                "type": if thinking_enabled { "enabled" } else { "disabled" },
            }),
        );
    }
}

// ── AgentPhase ────────────────────────────────────────────────────────────────

/// Indicates what the caller should do after the current step completes.
///
/// Inspect `state.phase` after receiving a terminal [`StepEvent`] to determine
/// the next action:
/// - [`Ready`](AgentPhase::Ready) — send a new user message
/// - [`AwaitingToolExecution`](AgentPhase::AwaitingToolExecution) — execute the listed tools
///   and call `step` again with `Action::ToolResults`
/// - [`Done`](AgentPhase::Done) — conversation complete
/// - [`Error`](AgentPhase::Error) — a non-recoverable error occurred
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AgentPhase {
    /// Ready for a new step (user message or tool results).
    Ready,
    /// Model requested tool calls — caller should execute and respond.
    AwaitingToolExecution { tool_calls: Vec<ParsedToolCall> },
    /// Conversation complete.
    Done,
    /// An error occurred.
    Error,
}

// ── Action ────────────────────────────────────────────────────────────────────

/// Caller-supplied input to a single [`step()`] call.
///
/// Use [`Action::UserMessage`] or [`Action::UserContent`] to start a new turn,
/// and [`Action::ToolResults`] to feed back the outcomes of tool calls requested
/// by the previous step.
///
/// ```ignore
/// // New user turn
/// Action::UserMessage("What is the weather in Paris?".into())
///
/// // Feed back tool results
/// Action::ToolResults(vec![
///     ToolCallOutcome::Result {
///         tool_call_id: "call-1".into(),
///         tool_name: "get_weather".into(),
///         content: Content::text("Sunny, 22°C"),
///     }
/// ])
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum Action {
    /// Start with a plain-text user message.
    UserMessage(String),
    /// Start with rich (multimodal) content.
    UserContent {
        content: Content,
        /// Optional metadata to attach to the created user message.
        message_metadata: Option<serde_json::Value>,
        /// Optional user identifier (`name` field) on the created user message.
        user_name: Option<String>,
    },
    /// Feed back tool execution results (response to `NeedToolExecution`).
    ToolResults(Vec<ToolCallOutcome>),
}

// ── StepEvent ─────────────────────────────────────────────────────────────────

/// Events streamed from a single [`step()`] call.
///
/// The stream always ends with exactly one *terminal* event (`Done`,
/// `NeedToolExecution`, or `Error`) which carries the updated [`AgentState`].
#[derive(Debug)]
pub enum StepEvent {
    // ── streaming deltas ──
    TextDelta(String),
    /// Emitted once when the first reasoning/thinking chunk arrives from a thinking model.
    /// All events until `ReasoningEnd` conceptually occur "inside" the thinking phase.
    ReasoningStart,
    /// Emitted when reasoning is complete (transition to content/tools or stream done).
    /// Carries the full accumulated chain-of-thought text.
    ReasoningEnd {
        content: String,
    },
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        id: String,
        delta: String,
    },
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
    },

    // ── terminal (exactly one, always last) ──
    /// Model responded with text; no tool calls.
    Done {
        state: AgentState,
    },
    /// Model requested tool calls. Execute externally, then call
    /// `step(state, Action::ToolResults(..), model)`.
    NeedToolExecution {
        state: AgentState,
        tool_calls: Vec<ParsedToolCall>,
    },
    /// An error occurred.
    Error {
        state: AgentState,
        error: AgentError,
    },
}

// ── Internal accumulator ──────────────────────────────────────────────────────

struct ToolCallAccumulator {
    #[allow(dead_code)]
    index: usize,
    id: String,
    name: String,
    arguments: String,
}

// ── step() ────────────────────────────────────────────────────────────────────

/// Pure, stateless step: one model call, streaming deltas, terminal event
/// with the new [`AgentState`].
///
/// # Panics
/// Does **not** panic. Model or transport errors are reported via
/// [`StepEvent::Error`].
pub fn step<M: ChatModel>(
    state: AgentState,
    action: Action,
    model: &M,
) -> impl Stream<Item = StepEvent> + '_ {
    let mut state = state;

    stream! {
        // ── 1. Apply the action to mutate state ──────────────────────
        match action {
            Action::UserMessage(text) => {
                // Ensure system prompt is present as the first message
                if let Some(ref sys) = state.system_prompt {
                    if !state.messages.first().is_some_and(|m| matches!(m.role, crate::types::Role::System)) {
                        state.messages.insert(0, Message::system(sys));
                    }
                }
                state.messages.push(Message::user(&text));
            }
            Action::UserContent { content, message_metadata, user_name } => {
                if let Some(ref sys) = state.system_prompt {
                    if !state.messages.first().is_some_and(|m| matches!(m.role, crate::types::Role::System)) {
                        state.messages.insert(0, Message::system(sys));
                    }
                }
                state.messages.push(Message {
                    id: crate::types::MessageId::new(),
                    role: crate::types::Role::User,
                    content,
                    tool_calls: None,
                    tool_call_id: None,
                    name: user_name,
                    reasoning_content: None,
                    metadata: message_metadata,
                });
            }
            Action::ToolResults(outcomes) => {
                for outcome in outcomes {
                    match outcome {
                        ToolCallOutcome::Result { tool_call_id, content, .. } => {
                            state.messages.push(Message::tool_result_content(&tool_call_id, content));
                        }
                        ToolCallOutcome::Error { tool_call_id, error, .. } => {
                            state.messages.push(Message::tool_result(&tool_call_id, &format!("error: {error}")));
                        }
                    }
                }
            }
        }

        // ── 2. Build ChatRequest ─────────────────────────────────────
        let tool_defs = if state.tool_definitions.is_empty() {
            None
        } else {
            Some(state.tool_definitions.clone())
        };

        let mut request = ChatRequest {
            model: state.config.model.clone(),
            messages: state.messages.clone(),
            tools: tool_defs,
            temperature: state.config.temperature,
            max_tokens: state.config.max_tokens,
            stream: true,
            stream_options: Some(StreamOptions { include_usage: true }),
            metadata: state.config.metadata.clone(),
            rate_limit_retry: state.config.rate_limit_retry.clone(),
            extra_body: state.config.extra_body.clone(),
        };
        apply_provider_request_overrides(&mut request);

        // ── 3. Call model ────────────────────────────────────────────
        let chat_stream = match model.chat(request).await {
            Ok(s) => s,
            Err(e) => {
                state.phase = AgentPhase::Error;
                yield StepEvent::Error { state, error: e };
                return;
            }
        };
        let mut chat_stream = std::pin::pin!(chat_stream);

        // ── 4. Stream model chunks ───────────────────────────────────
        let mut tool_accumulators: Vec<ToolCallAccumulator> = Vec::new();
        let mut index_map: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        let mut text_parts: Vec<String> = Vec::new();
        let mut reasoning_parts: Vec<String> = Vec::new();
        // Bracket tracking: emit ReasoningStart on first chunk, ReasoningEnd on transition.
        let mut reasoning_open = false; // ReasoningStart emitted, ReasoningEnd not yet

        while let Some(chunk) = chat_stream.next().await {
            match chunk {
                ChatResponseChunk::ReasoningDelta { content } => {
                    if !reasoning_open {
                        reasoning_open = true;
                        yield StepEvent::ReasoningStart;
                    }
                    reasoning_parts.push(content);
                }
                ChatResponseChunk::Delta { content, .. } => {
                    if reasoning_open {
                        reasoning_open = false;
                        yield StepEvent::ReasoningEnd { content: reasoning_parts.join("") };
                    }
                    text_parts.push(content.clone());
                    yield StepEvent::TextDelta(content);
                }
                ChatResponseChunk::ToolCallStart { index, id, name } => {
                    if reasoning_open {
                        reasoning_open = false;
                        yield StepEvent::ReasoningEnd { content: reasoning_parts.join("") };
                    }
                    yield StepEvent::ToolCallStart { id: id.clone(), name: name.clone() };
                    let pos = tool_accumulators.len();
                    tool_accumulators.push(ToolCallAccumulator { index, id, name, arguments: String::new() });
                    index_map.insert(index, pos);
                }
                ChatResponseChunk::ToolCallDelta { index, arguments_delta } => {
                    if let Some(&pos) = index_map.get(&index) {
                        let tc = &mut tool_accumulators[pos];
                        yield StepEvent::ToolCallArgumentsDelta {
                            id: tc.id.clone(),
                            delta: arguments_delta.clone(),
                        };
                        tc.arguments.push_str(&arguments_delta);
                    }
                }
                ChatResponseChunk::Usage { prompt_tokens, completion_tokens, .. } => {
                    yield StepEvent::Usage { prompt_tokens, completion_tokens };
                }
                ChatResponseChunk::Done => break,
            }
        }

        // ── 5. Terminal event ────────────────────────────────────────

        // Close reasoning block if model ended without emitting content/tools
        if reasoning_open {
            yield StepEvent::ReasoningEnd { content: reasoning_parts.join("") };
        }

        let reasoning_content = if reasoning_parts.is_empty() { None } else { Some(reasoning_parts.join("")) };

        if tool_accumulators.is_empty() {
            // No tool calls — assistant text response
            let text = text_parts.join("");
            let mut msg = Message::assistant(&text);
            msg.reasoning_content = reasoning_content;
            state.messages.push(msg);
            state.phase = AgentPhase::Done;
            yield StepEvent::Done { state };
        } else {
            // Tool calls — build assistant message with tool_calls
            let tool_call_messages: Vec<ToolCallMessage> = tool_accumulators.iter().map(|tc| {
                ToolCallMessage {
                    id: tc.id.clone(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                    },
                }
            }).collect();

            let text = text_parts.join("");
            state.messages.push(Message::assistant_with_tool_calls(text, tool_call_messages, reasoning_content));

            let parsed: Vec<ParsedToolCall> = tool_accumulators.into_iter()
                .map(|tc| ParsedToolCall {
                    id: tc.id,
                    name: tc.name,
                    arguments: serde_json::from_str(&tc.arguments).unwrap_or(serde_json::Value::Null),
                })
                .collect();

            state.phase = AgentPhase::AwaitingToolExecution { tool_calls: parsed.clone() };
            yield StepEvent::NeedToolExecution { state, tool_calls: parsed };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_provider_request_overrides, metadata_flag};
    use crate::types::{ChatRequest, Message};
    use serde_json::json;

    fn request(model: &str, metadata: Option<serde_json::Value>) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![Message::user("hello")],
            tools: None,
            temperature: None,
            max_tokens: None,
            stream: true,
            stream_options: None,
            metadata,
            rate_limit_retry: None,
            extra_body: serde_json::Map::new(),
        }
    }

    #[test]
    fn metadata_flag_accepts_bool_and_string_values() {
        assert_eq!(
            metadata_flag(&json!({ "thinking_enabled": true }), "thinking_enabled"),
            Some(true)
        );
        assert_eq!(
            metadata_flag(&json!({ "thinking_enabled": false }), "thinking_enabled"),
            Some(false)
        );
        assert_eq!(
            metadata_flag(
                &json!({ "thinking_enabled": "enabled" }),
                "thinking_enabled"
            ),
            Some(true)
        );
        assert_eq!(
            metadata_flag(
                &json!({ "thinking_enabled": "disabled" }),
                "thinking_enabled"
            ),
            Some(false)
        );
    }

    #[test]
    fn apply_provider_request_overrides_disables_kimi_thinking() {
        let mut request = request("kimi-k2.5", Some(json!({ "thinking_enabled": false })));
        apply_provider_request_overrides(&mut request);
        assert_eq!(
            request.extra_body.get("thinking"),
            Some(&json!({ "type": "disabled" }))
        );
    }

    #[test]
    fn apply_provider_request_overrides_leaves_other_models_unchanged() {
        let mut request = request("gpt-4o", Some(json!({ "thinking_enabled": false })));
        apply_provider_request_overrides(&mut request);
        assert!(!request.extra_body.contains_key("thinking"));
    }
}
