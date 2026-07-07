use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use uuid::Uuid;

fn uuid_v4() -> String {
    Uuid::new_v4().to_string()
}

// ── Identifiers ──────────────────────────────────────────────────────────────

/// Unique identifier for a conversation thread.
///
/// A thread holds multiple runs and messages.  Create one via
/// [`BuiltAgent::create_thread`](crate::builder::BuiltAgent) or
/// [`ContextStore::create_thread`](crate::context::ContextStore).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadId(pub String);

/// Unique identifier for a single run.
///
/// A run corresponds to one `agent.chat()` call.  The same `RunId` is
/// retained across interrupt/resume cycles that belong to the same logical
/// invocation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RunId(pub String);

/// Unique identifier for a single message within a thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

/// Unique identifier for a single tool interrupt instance.
///
/// Returned inside [`InterruptRequest`](crate::tool::InterruptRequest) and
/// used in [`ResumePayload`] to match a resume response to its interrupt.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InterruptId(pub String);

impl ThreadId {
    pub fn new() -> Self {
        Self(uuid_v4())
    }
}
impl RunId {
    pub fn new() -> Self {
        Self(uuid_v4())
    }
}
impl MessageId {
    pub fn new() -> Self {
        Self(uuid_v4())
    }
}
impl InterruptId {
    pub fn new() -> Self {
        Self(uuid_v4())
    }
}

impl Default for ThreadId {
    fn default() -> Self {
        Self::new()
    }
}
impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}
impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}
impl Default for InterruptId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl fmt::Display for InterruptId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Multimodal Content ────────────────────────────────────────────────────────

/// The content of a message, compatible with the OpenAI `content` field.
///
/// Can be plain text (`Text`) or a sequence of typed parts (`Parts`).
/// Use [`Content::text`] for the common text-only case and
/// [`Content::parts`] when mixing text with images or audio.
///
/// # Examples
///
/// ```ignore
/// use remi_agentloop_core::types::{Content, ContentPart};
///
/// // Plain text
/// let c = Content::text("Hello!");
///
/// // Mixed image + text
/// let c = Content::parts(vec![
///     ContentPart::text("Describe this image:"),
///     ContentPart::image_url("https://example.com/photo.jpg"),
/// ]);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Content {
    pub fn text(s: impl Into<String>) -> Self {
        Content::Text(s.into())
    }
    pub fn parts(parts: Vec<ContentPart>) -> Self {
        Content::Parts(parts)
    }

    pub fn text_content(&self) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        }
    }

    pub fn is_multimodal(&self) -> bool {
        matches!(self, Content::Parts(parts) if parts.iter().any(|p| !matches!(p, ContentPart::Text { .. })))
    }
}

/// A single typed content part inside a [`Content::Parts`] message.
///
/// Corresponds to an OpenAI multimodal `content` part object.
/// Construct parts using the associated helper methods:
/// [`ContentPart::text`], [`ContentPart::image_url`], [`ContentPart::image_base64`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },

    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlDetail },

    #[serde(rename = "image_base64")]
    ImageBase64 { media_type: String, data: String },

    #[serde(rename = "input_audio")]
    Audio { input_audio: AudioDetail },

    #[serde(rename = "file")]
    File {
        file_id: Option<String>,
        filename: Option<String>,
        media_type: Option<String>,
        data: Option<String>,
    },
}

impl ContentPart {
    pub fn text(s: impl Into<String>) -> Self {
        ContentPart::Text { text: s.into() }
    }
    pub fn image_url(url: impl Into<String>) -> Self {
        ContentPart::ImageUrl {
            image_url: ImageUrlDetail {
                url: url.into(),
                detail: None,
            },
        }
    }
    pub fn image_base64(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        ContentPart::ImageBase64 {
            media_type: media_type.into(),
            data: data.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrlDetail {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDetail {
    pub data: String,
    pub format: String,
}

// ── Role & Message ────────────────────────────────────────────────────────────

/// The role of a participant in a conversation.
///
/// Matches the OpenAI Chat Completions `role` field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A system-level instruction (sent before the first user message).
    System,
    /// A message from the human user.
    User,
    /// A reply generated by the language model.
    Assistant,
    /// The result of a model-requested tool call.
    Tool,
}

/// A single message in a conversation thread.
///
/// Use the constructor helpers for the most common cases:
/// [`Message::user`], [`Message::assistant`], [`Message::system`],
/// [`Message::tool_result`].
///
/// # Example
///
/// ```ignore
/// use remi_agentloop_core::types::Message;
///
/// let history = vec![
///     Message::system("You are a concise assistant."),
///     Message::user("What is the capital of France?"),
///     Message::assistant("Paris."),
/// ];
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub id: MessageId,
    pub role: Role,
    pub content: Content,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional user identifier for `Role::User` messages.
    ///
    /// Maps to the `name` field in OpenAI-compatible request bodies.
    /// Useful for multi-user scenarios or end-user abuse monitoring.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Chain-of-thought / reasoning text returned by thinking models (e.g. Kimi K2.5).
    /// Must be echoed back verbatim when replaying the conversation history.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// User-defined metadata attached to this message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::User,
            content: Content::text(text),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            metadata: None,
        }
    }

    /// Create a user message with an explicit user identifier.
    ///
    /// The `user_id` is serialised as the `name` field in OpenAI-compatible
    /// request bodies, useful for multi-user conversations.
    pub fn user_with_id(text: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::User,
            content: Content::text(text),
            tool_calls: None,
            tool_call_id: None,
            name: Some(user_id.into()),
            reasoning_content: None,
            metadata: None,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::System,
            content: Content::text(text),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            metadata: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::Assistant,
            content: Content::text(text),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            metadata: None,
        }
    }

    pub fn assistant_with_tool_calls(
        text: impl Into<String>,
        tool_calls: Vec<ToolCallMessage>,
        reasoning_content: Option<String>,
    ) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::Assistant,
            content: Content::text(text),
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            name: None,
            reasoning_content,
            metadata: None,
        }
    }

    /// Set the user identifier on this message (maps to the `name` field).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the user identifier from an `Option` — no-op if `None`.
    pub fn with_name_opt(mut self, name: Option<String>) -> Self {
        if let Some(n) = name {
            self.name = Some(n);
        }
        self
    }

    /// Attach user-defined metadata to this message.
    pub fn with_metadata(mut self, metadata: impl Into<Value>) -> Self {
        self.metadata = Some(metadata.into());
        self
    }

    pub fn tool_result(tool_call_id: impl Into<String>, result: impl Into<String>) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::Tool,
            content: Content::text(result),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            name: None,
            reasoning_content: None,
            metadata: None,
        }
    }

    /// Tool result with rich content (text and/or images).
    pub fn tool_result_content(tool_call_id: impl Into<String>, content: Content) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::Tool,
            content,
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            name: None,
            reasoning_content: None,
            metadata: None,
        }
    }

    pub fn user_multimodal(parts: Vec<ContentPart>) -> Self {
        Self {
            id: MessageId::new(),
            role: Role::User,
            content: Content::parts(parts),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_content: None,
            metadata: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

// ── ChatRequest / ChatResponseChunk ──────────────────────────────────────────

use crate::config::RateLimitRetryPolicy;
use crate::tool::ToolDefinition;

#[derive(Debug, Clone, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Internal retry policy for rate-limited model calls.
    #[serde(skip)]
    pub rate_limit_retry: Option<RateLimitRetryPolicy>,
    /// Provider-specific extra parameters merged into the top-level request body.
    ///
    /// Keys are flattened directly into the JSON object, enabling any
    /// OpenAI-compatible parameter not otherwise modelled here (e.g.
    /// `top_p`, `presence_penalty`, vendor extensions).
    /// Populated from [`AgentBuilder::extra_options`].
    #[serde(flatten)]
    pub extra_body: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub enum ChatResponseChunk {
    Delta {
        content: String,
        role: Option<Role>,
    },
    /// Chain-of-thought / thinking content from reasoning models (e.g. Kimi K2.5, DeepSeek-R1).
    ReasoningDelta {
        content: String,
    },
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolCallDelta {
        index: usize,
        arguments_delta: String,
    },
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
        total_tokens: u32,
    },
    Done,
}

// ── AgentEvent ────────────────────────────────────────────────────────────────

use crate::error::AgentError;

/// Events streamed from the agent loop to the caller.
///
/// Consume these by polling the stream returned from `agent.chat(…)`.
/// The stream always terminates with [`AgentEvent::Done`],
/// [`AgentEvent::Interrupt`], or [`AgentEvent::Error`].
///
/// # Pattern match reference
///
/// ```ignore
/// while let Some(event) = stream.next().await {
///     match event {
///         AgentEvent::TextDelta(chunk)  => print!("{chunk}"),
///         AgentEvent::ToolCallStart { name, .. } => println!("[{name}]"),
///         AgentEvent::ToolResult { name, result, .. } => println!("→ {result}"),
///         AgentEvent::Done              => break,
///         AgentEvent::Error(e)          => return Err(e.into()),
///         _                             => {}
///     }
/// }
/// ```
#[derive(Debug, Clone)]
pub enum AgentEvent {
    RunStart {
        thread_id: ThreadId,
        run_id: RunId,
        metadata: Option<serde_json::Value>,
    },
    TextDelta(String),
    /// Emitted once when a thinking model begins its chain-of-thought.
    /// All events until `ThinkingEnd` occur conceptually inside the thinking phase.
    ThinkingStart,
    /// Emitted when the thinking phase ends. Carries the full accumulated reasoning text.
    ThinkingEnd {
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
    ToolDelta {
        id: String,
        name: String,
        delta: String,
    },
    ToolResult {
        id: String,
        name: String,
        result: String,
    },
    SubSession(SubSessionEvent),
    Interrupt {
        interrupts: Vec<InterruptInfo>,
    },
    TurnStart {
        turn: usize,
    },
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
    },
    Done,
    /// The run was cancelled by the user.  A `Cancelled` checkpoint has been
    /// saved; the conversation can be resumed from where it was interrupted.
    Cancelled,
    Error(AgentError),
    /// Full state checkpoint emitted at key lifecycle boundaries.
    /// Outer layers (e.g. `BuiltAgent`) intercept this for durable persistence
    /// and filter it out before reaching the consumer.
    ///
    /// Contains everything needed to resume execution after a crash or restart.
    Checkpoint(crate::checkpoint::Checkpoint),
    /// Tool calls that the inner agent loop cannot execute (not in its registry).
    /// The outer layer should execute these externally, then resume via
    /// `AgentLoop::run(state, Action::ToolResults(all_outcomes), false)`.
    ///
    /// `completed_results` contains outcomes of tools that **were** executed
    /// internally by this loop. The outer layer must merge its own results
    /// with these before resuming.
    NeedToolExecution {
        state: crate::state::AgentState,
        tool_calls: Vec<ParsedToolCall>,
        completed_results: Vec<ToolCallOutcome>,
    },
}

pub fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubSessionEvent {
    pub parent_tool_call_id: String,
    pub sub_thread_id: ThreadId,
    pub sub_run_id: RunId,
    pub agent_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub depth: u32,
    #[serde(flatten)]
    pub payload: SubSessionEventPayload,
}

impl SubSessionEvent {
    pub fn new(
        parent_tool_call_id: impl Into<String>,
        sub_thread_id: ThreadId,
        sub_run_id: RunId,
        agent_name: impl Into<String>,
        title: Option<String>,
        depth: u32,
        payload: SubSessionEventPayload,
    ) -> Self {
        Self {
            parent_tool_call_id: parent_tool_call_id.into(),
            sub_thread_id,
            sub_run_id,
            agent_name: agent_name.into(),
            title,
            depth,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "sub_type", rename_all = "snake_case")]
pub enum SubSessionEventPayload {
    Start,
    Delta {
        content: String,
    },
    ThinkingStart,
    ThinkingEnd {
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
    ToolDelta {
        id: String,
        name: String,
        delta: String,
    },
    ToolResult {
        id: String,
        name: String,
        result: String,
    },
    TurnStart {
        turn: usize,
    },
    Done {
        #[serde(skip_serializing_if = "Option::is_none")]
        final_output: Option<String>,
    },
    Error {
        message: String,
    },
}

/// Details of a single tool interrupt — part of [`AgentEvent::Interrupt`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptInfo {
    pub interrupt_id: InterruptId,
    pub tool_call_id: String,
    pub tool_name: String,
    pub kind: String,
    pub data: serde_json::Value,
}

/// Data provided by the caller when resuming after an [`InterruptRequest`](crate::tool::InterruptRequest).
///
/// # Example
///
/// ```ignore
/// use remi_agentloop_core::types::ResumePayload;
///
/// // After collecting the user's confirmation:
/// let payloads = vec![
///     ResumePayload {
///         interrupt_id: interrupt_info.interrupt_id.clone(),
///         result: serde_json::json!({ "approved": true }),
///     },
/// ];
/// agent.chat(ChatInput::resume(payloads)).await?;
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumePayload {
    pub interrupt_id: InterruptId,
    pub result: serde_json::Value,
}

/// Versioned export of a chat/debug session for later replay or inspection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSessionBundle {
    pub version: u32,
    pub exported_at: chrono::DateTime<chrono::Utc>,
    pub thread_id: ThreadId,
    pub run_id: RunId,
    pub replay: ChatReplayCursor,
    pub state: crate::state::AgentState,
    #[serde(default)]
    pub checkpoints: Vec<crate::checkpoint::Checkpoint>,
    #[serde(default)]
    pub metadata: serde_json::Map<String, Value>,
}

impl ChatSessionBundle {
    pub const VERSION: u32 = 1;

    pub fn new(state: crate::state::AgentState) -> Self {
        let replay = ChatReplayCursor::from_state(&state);
        Self {
            version: Self::VERSION,
            exported_at: chrono::Utc::now(),
            thread_id: state.thread_id.clone(),
            run_id: state.run_id.clone(),
            replay,
            state,
            checkpoints: Vec::new(),
            metadata: serde_json::Map::new(),
        }
    }

    pub fn with_checkpoints(mut self, checkpoints: Vec<crate::checkpoint::Checkpoint>) -> Self {
        self.checkpoints = checkpoints;
        self
    }

    pub fn with_metadata(mut self, metadata: serde_json::Map<String, Value>) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Location in the exported conversation history from which replay should start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatReplayCursor {
    pub start_message_index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_message_id: Option<MessageId>,
}

impl ChatReplayCursor {
    pub fn from_state(state: &crate::state::AgentState) -> Self {
        let start_message_index = state.messages.len().saturating_sub(1);
        let start_message_id = state
            .messages
            .get(start_message_index)
            .map(|msg| msg.id.clone());
        Self {
            start_message_index,
            start_message_id,
        }
    }
}

// ── Internal loop types (pub(crate)) ─────────────────────────────────────────

/// Parsed and fully accumulated tool call ready for execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Single tool call execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub id: String,
    pub name: String,
    pub result: String,
}

/// Outcome of executing a tool call externally.
///
/// Used with [`Action::ToolResults`](crate::state::Action::ToolResults) and
/// [`LoopInput::Resume`] to feed tool results back into the agent loop when
/// tool execution happens outside `AgentLoop` (e.g. in a composable outer layer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolCallOutcome {
    /// Tool executed successfully (content may include text and/or images)
    Result {
        tool_call_id: String,
        tool_name: String,
        content: Content,
    },
    /// Tool execution failed
    Error {
        tool_call_id: String,
        tool_name: String,
        error: String,
    },
}

// ── LoopInput ─────────────────────────────────────────────────────────────────

/// Unified input for `Agent::chat()` — used by `AgentLoop`, composable layers,
/// and the protocol/transport layer.
///
/// Merges the previous `LoopInput` and `ProtocolRequest` into a single
/// serialisable type that supports:
/// - Starting a new turn with text or multimodal content
/// - Resuming after `NeedToolExecution`
/// - Protocol-level overrides (model, temperature, max_tokens, metadata)
///
/// ```ignore
/// // Start a new conversation (String converts automatically):
/// agent.chat("hello".into()).await?;
///
/// // Start with multimodal content:
/// agent.chat(Content::parts(vec![
///     ContentPart::text("describe this image"),
///     ContentPart::image_url("https://example.com/img.png"),
/// ]).into()).await?;
///
/// // Start with history + extra tool definitions + overrides:
/// agent.chat(
///     LoopInput::start("hello")
///         .history(msgs)
///         .extra_tools(defs)
///         .model("gpt-4o")
///         .temperature(0.5)
/// ).await?;
///
/// // Resume after NeedToolExecution:
/// agent.chat(LoopInput::resume(state, outcomes)).await?;
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LoopInput {
    /// Start a new conversation turn
    #[serde(rename = "start")]
    Start {
        /// User message content — text or multimodal
        content: Content,
        /// Conversation history from prior turns
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        history: Vec<Message>,
        /// Additional tool definitions injected by outer layers
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        extra_tools: Vec<crate::tool::ToolDefinition>,
        /// Override model name for this request
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Override temperature for this request
        #[serde(skip_serializing_if = "Option::is_none")]
        temperature: Option<f64>,
        /// Override max tokens for this request
        #[serde(skip_serializing_if = "Option::is_none")]
        max_tokens: Option<u32>,
        /// Request metadata
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
        /// Metadata to attach to the user message created from `content`.
        /// Stored alongside the message in conversation history.
        #[serde(skip_serializing_if = "Option::is_none")]
        message_metadata: Option<Value>,
        /// Optional user identifier attached to the user message as the `name` field.
        ///
        /// Useful in multi-user scenarios — the LLM sees `role: "user"` plus
        /// `name: "<user_id>"` as separate fields in the request body.
        #[serde(skip_serializing_if = "Option::is_none")]
        user_name: Option<String>,
        /// Initial user_state to inject (tool-managed per-thread state, e.g. todos)
        #[serde(skip_serializing_if = "Option::is_none")]
        user_state: Option<serde_json::Value>,
    },
    /// Resume from a `NeedToolExecution` with completed tool results
    #[serde(rename = "resume")]
    Resume {
        state: crate::state::AgentState,
        results: Vec<ToolCallOutcome>,
    },
    /// Cancel an in-progress run.  Produces a `Cancelled` checkpoint so the
    /// conversation can be resumed later.
    ///
    /// This is the **low-level** cancellation path used when you drive
    /// [`AgentLoop`] directly and already hold the [`AgentState`].  If you
    /// are using the high-level [`BuiltAgent`] / [`crate::BuiltAgent`] API,
    /// use [`ChatInput::Cancel`] instead — it takes only a `run_id` and
    /// accepts an optional [`ChatInput::Cancel::partial_response`] string to
    /// persist text that was already streamed before the cancel arrived.
    ///
    /// [`AgentState`]: crate::state::AgentState
    /// [`AgentLoop`]: crate::AgentLoop
    #[serde(rename = "cancel")]
    Cancel { state: crate::state::AgentState },
}

impl LoopInput {
    /// Create a `Start` input with a text message.
    pub fn start(msg: impl Into<String>) -> Self {
        Self::Start {
            content: Content::text(msg),
            history: vec![],
            extra_tools: vec![],
            model: None,
            temperature: None,
            max_tokens: None,
            metadata: None,
            message_metadata: None,
            user_name: None,
            user_state: None,
        }
    }

    /// Create a `Start` input with multimodal content.
    pub fn start_content(content: Content) -> Self {
        Self::Start {
            content,
            history: vec![],
            extra_tools: vec![],
            model: None,
            temperature: None,
            max_tokens: None,
            metadata: None,
            message_metadata: None,
            user_name: None,
            user_state: None,
        }
    }

    /// Create a `Resume` input from state + tool results.
    pub fn resume(state: crate::state::AgentState, results: Vec<ToolCallOutcome>) -> Self {
        Self::Resume { state, results }
    }

    /// Create a `Cancel` input to abort a running conversation.
    pub fn cancel(state: crate::state::AgentState) -> Self {
        Self::Cancel { state }
    }

    /// Builder: attach conversation history (only applies to `Start`).
    pub fn history(mut self, msgs: Vec<Message>) -> Self {
        if let Self::Start { history, .. } = &mut self {
            *history = msgs;
        }
        self
    }

    /// Builder: attach extra tool definitions (only applies to `Start`).
    pub fn extra_tools(mut self, defs: Vec<crate::tool::ToolDefinition>) -> Self {
        if let Self::Start { extra_tools, .. } = &mut self {
            *extra_tools = defs;
        }
        self
    }

    /// Builder: override model name (only applies to `Start`).
    pub fn model(mut self, m: impl Into<String>) -> Self {
        if let Self::Start { model, .. } = &mut self {
            *model = Some(m.into());
        }
        self
    }

    /// Builder: override temperature (only applies to `Start`).
    pub fn temperature(mut self, t: f64) -> Self {
        if let Self::Start { temperature, .. } = &mut self {
            *temperature = Some(t);
        }
        self
    }

    /// Builder: override max tokens (only applies to `Start`).
    pub fn max_tokens(mut self, n: u32) -> Self {
        if let Self::Start { max_tokens, .. } = &mut self {
            *max_tokens = Some(n);
        }
        self
    }

    /// Builder: set metadata (only applies to `Start`).
    pub fn metadata(mut self, v: serde_json::Value) -> Self {
        if let Self::Start { metadata, .. } = &mut self {
            *metadata = Some(v);
        }
        self
    }

    /// Builder: set metadata on the user message created from `content` (only applies to `Start`).
    pub fn message_metadata(mut self, v: serde_json::Value) -> Self {
        if let Self::Start {
            message_metadata, ..
        } = &mut self
        {
            *message_metadata = Some(v);
        }
        self
    }

    /// Builder: set the user identifier on the user message (only applies to `Start`).
    ///
    /// Serialised as the `name` field in OpenAI-compatible request bodies.
    pub fn user_name(mut self, name: impl Into<String>) -> Self {
        if let Self::Start { user_name, .. } = &mut self {
            *user_name = Some(name.into());
        }
        self
    }

    /// Builder: set initial user_state (only applies to `Start`).
    pub fn user_state(mut self, v: serde_json::Value) -> Self {
        if let Self::Start { user_state, .. } = &mut self {
            *user_state = Some(v);
        }
        self
    }
}

impl From<String> for LoopInput {
    fn from(s: String) -> Self {
        Self::start(s)
    }
}

impl From<&str> for LoopInput {
    fn from(s: &str) -> Self {
        Self::start(s)
    }
}

impl From<Content> for LoopInput {
    fn from(c: Content) -> Self {
        Self::start_content(c)
    }
}

// ── ChatInput ─────────────────────────────────────────────────────────────────

/// Unified input for `chat_in_thread` — covers both new messages and resume from interrupt.
///
/// ```ignore
/// // New user message (String converts automatically):
/// agent.chat_in_thread(&tid, "hello").await?;
///
/// // Resume from interrupt:
/// agent.chat_in_thread(&tid, ChatInput::Resume {
///     run_id,
///     completed_results: vec![],
///     pending_interrupts: interrupts,
///     payloads: vec![payload],
/// }).await?;
/// ```
#[derive(Debug, Clone)]
pub enum ChatInput {
    /// A new user message (text or multimodal)
    Message {
        content: Content,
        /// Optional user identifier — serialised as the `name` field in the request body.
        user_name: Option<String>,
    },
    /// Resume a previously interrupted run
    Resume {
        run_id: RunId,
        /// Tool calls that completed normally (before the interrupt)
        completed_results: Vec<ToolCallResult>,
        /// The interrupt(s) that were returned by the agent
        pending_interrupts: Vec<InterruptInfo>,
        /// User-provided payloads resolving each interrupt
        payloads: Vec<ResumePayload>,
    },
    /// Cancel an in-progress run.  Saves a `Cancelled` checkpoint and
    /// yields [`AgentEvent::Cancelled`] so the conversation can be resumed
    /// later via [`ChatInput::Resume`].
    ///
    /// # Partial streaming output
    ///
    /// When a user interrupts an active stream (e.g. by pressing a stop
    /// button), the LLM may already have emitted several [`AgentEvent::TextDelta`]
    /// events that have been displayed but not yet committed to a checkpoint.
    /// Pass those accumulated deltas as `partial_response` so they are
    /// persisted as an incomplete assistant message before the checkpoint is
    /// saved.  The caller is responsible for accumulating the deltas:
    ///
    /// ```ignore
    /// let mut partial = String::new();
    ///
    /// // Drive the active stream until the user signals cancellation.
    /// loop {
    ///     tokio::select! {
    ///         Some(event) = stream.next() => {
    ///             if let AgentEvent::TextDelta { delta, .. } = &event {
    ///                 partial.push_str(delta);
    ///             }
    ///             render(event);
    ///         }
    ///         _ = cancel_signal.notified() => break,
    ///     }
    /// }
    ///
    /// // Send the accumulated text back so the framework persists it.
    /// agent.chat(ChatInput::Cancel {
    ///     run_id,
    ///     partial_response: Some(partial).filter(|s| !s.is_empty()),
    /// }).await;
    /// ```
    ///
    /// After the cancel completes the thread's message list will contain the
    /// partial assistant turn, so a future [`ChatInput::Resume`] gives the
    /// model full context.
    ///
    /// > **WASM / single-threaded hosts**: `tokio::select!` is unavailable.
    /// > Check a cancellation flag between `call_next()` iterations instead
    /// > and pass the accumulated output the same way.
    Cancel {
        run_id: RunId,
        /// Text already streamed in the current (incomplete) turn, if any.
        /// See the [`ChatInput::Cancel`] doc-comment for the accumulation pattern.
        #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
        partial_response: Option<String>,
    },
}

impl ChatInput {
    /// Create a plain text message input.
    pub fn text(msg: impl Into<String>) -> Self {
        ChatInput::Message {
            content: Content::text(msg),
            user_name: None,
        }
    }

    /// Create a multimodal message input (text + images, audio, etc.).
    pub fn multimodal(parts: Vec<ContentPart>) -> Self {
        ChatInput::Message {
            content: Content::parts(parts),
            user_name: None,
        }
    }

    /// Attach a user identifier to a `Message` input.
    pub fn with_user_name(self, name: impl Into<String>) -> Self {
        match self {
            ChatInput::Message { content, .. } => ChatInput::Message {
                content,
                user_name: Some(name.into()),
            },
            other => other,
        }
    }
}

impl From<String> for ChatInput {
    fn from(s: String) -> Self {
        ChatInput::Message {
            content: Content::text(s),
            user_name: None,
        }
    }
}

impl From<&str> for ChatInput {
    fn from(s: &str) -> Self {
        ChatInput::Message {
            content: Content::text(s),
            user_name: None,
        }
    }
}
