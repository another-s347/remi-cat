use crate::config::AgentConfig;
use crate::error::AgentError;
use crate::types::{Content, InterruptId, ResumePayload, RunId, ThreadId};
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

// ── ToolOutput ────────────────────────────────────────────────────────────────

/// A single item streamed from a tool execution.
///
/// Tools can emit zero or more [`Delta`](ToolOutput::Delta) items (for live
/// progress display) followed by exactly one [`Result`](ToolOutput::Result)
/// item that carries the final content fed back into the conversation.
///
/// # Example
///
/// ```ignore
/// use remi_agentloop_core::tool::ToolOutput;
/// use futures::stream;
///
/// // A tool that streams three progress lines then a final result
/// let output = stream::iter(vec![
///     ToolOutput::Delta("Searching…\n".into()),
///     ToolOutput::Delta("Found 3 results.\n".into()),
///     ToolOutput::text("The answer is 42."),
/// ]);
/// ```
#[derive(Debug, Clone)]
pub enum ToolOutput {
    /// Live progress or intermediate text — shown to the user but not added
    /// to the conversation history.
    Delta(String),
    /// Structured sub-session event emitted by a sub-agent tool.
    SubSession(crate::types::SubSessionEvent),
    /// Final result content (text and/or images) — appended to the conversation
    /// as a tool-result message.
    Result(Content),
}

impl ToolOutput {
    /// Convenience constructor: a plain-text final result.
    ///
    /// ```ignore
    /// ToolOutput::text("42")
    /// // equivalent to:
    /// ToolOutput::Result(Content::text("42"))
    /// ```
    pub fn text(s: impl Into<String>) -> Self {
        ToolOutput::Result(Content::text(s))
    }
}

// ── ToolContext ────────────────────────────────────────────────────────────────

/// Runtime context injected by the agent loop into every tool execution.
///
/// Tools that don't need any context can simply ignore the `ctx` parameter.
/// Tools that need to read or write cross-tool shared state can access
/// [`user_state`](ToolContext::user_state).
///
/// # Example — reading config inside a tool
///
/// ```ignore
/// async fn execute(&self, args: serde_json::Value, _resume: Option<ResumePayload>, ctx: &ToolContext)
///     -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>
/// {
///     let base_url = ctx.config.base_url.as_deref().unwrap_or("https://api.example.com");
///     // … use base_url …
///     Ok(ToolResult::Output(stream::once(async { ToolOutput::text("ok") })))
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Runtime configuration for the current agent invocation (API key, model,
    /// base URL, timeout, …).
    pub config: AgentConfig,
    /// Thread identifier — `None` when the agent runs without a context store.
    pub thread_id: Option<ThreadId>,
    /// Current run identifier.
    pub run_id: RunId,
    /// Request-level metadata forwarded from [`LoopInput`](crate::types::LoopInput).
    pub metadata: Option<serde_json::Value>,
    /// Cooperative cancellation signal for the current run.
    ///
    /// Long-running tools should listen to this signal and terminate any child
    /// work they own when it fires.
    pub cancel: Option<Arc<tokio::sync::Notify>>,
    /// Opaque user-defined state shared across all tool calls in a single run.
    ///
    /// `AgentLoop` snapshots `AgentState.user_state` into this field before
    /// each batch of tool calls, then writes it back after the batch completes.
    /// This enables patterns like progressive disclosure, where Tool A unlocks
    /// Tool B by setting a flag in `user_state`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Inside a tool that unlocks the next tool:
    /// let mut state = ctx.user_state.write().unwrap();
    /// state["step_done"] = serde_json::json!(true);
    /// ```
    pub user_state: Arc<RwLock<serde_json::Value>>,
}

/// Runtime context used when generating tool definitions for the model.
///
/// Unlike [`ToolContext`], this snapshot is read-only and may be built before
/// a tool ever executes. Tools can inspect it to append optional, per-request
/// guidance to their advertised definition.
#[derive(Debug, Clone, Default)]
pub struct ToolDefinitionContext {
    /// Thread identifier — `None` when unavailable.
    pub thread_id: Option<ThreadId>,
    /// Current run identifier — `None` when definitions are generated before a
    /// run is created by an outer layer.
    pub run_id: Option<RunId>,
    /// Request-level metadata forwarded from [`LoopInput`](crate::types::LoopInput).
    pub metadata: Option<serde_json::Value>,
    /// Snapshot of the current user-managed tool state.
    pub user_state: serde_json::Value,
}

impl ToolDefinitionContext {
    pub fn from_user_state(user_state: serde_json::Value) -> Self {
        Self {
            user_state,
            ..Self::default()
        }
    }
}

// ── InterruptRequest ──────────────────────────────────────────────────────────

/// A request to pause the agent loop and wait for external input.
///
/// Return this from a tool's [`execute`](Tool::execute) method via
/// [`ToolResult::Interrupt`] when the tool requires human approval, payment
/// confirmation, policy check, or any other out-of-band action.
///
/// The agent loop pauses after collecting all interrupt requests, emits
/// [`AgentEvent::Interrupt`](crate::types::AgentEvent::Interrupt), and saves
/// a resumable [`Checkpoint`](crate::checkpoint::Checkpoint).
///
/// The caller resumes by calling `agent.chat(ChatInput::resume(interrupts))`
/// with [`ResumePayload`](crate::types::ResumePayload) values for each
/// interrupt.
///
/// # Example
///
/// ```ignore
/// use remi_agentloop_core::tool::InterruptRequest;
/// use serde_json::json;
///
/// // Inside a payment tool:
/// let interrupt = InterruptRequest::new(
///     "payment_confirm",
///     json!({ "amount": 99.99, "currency": "USD" }),
/// );
/// return Ok(ToolResult::Interrupt(interrupt));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptRequest {
    /// Unique identifier for this interrupt instance.
    pub interrupt_id: InterruptId,
    /// Interrupt kind — a short string that callers use to route the request.
    /// Examples: `"human_approval"`, `"payment_confirm"`, `"policy_check"`.
    pub kind: String,
    /// Arbitrary JSON payload forwarded to the waiting caller.
    pub data: serde_json::Value,
}

impl InterruptRequest {
    /// Create a new interrupt request with a fresh [`InterruptId`].
    pub fn new(kind: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            interrupt_id: InterruptId::new(),
            kind: kind.into(),
            data,
        }
    }
}

// ── ToolResult ────────────────────────────────────────────────────────────────

/// The outcome of a tool execution — either streaming output or an interrupt.
///
/// This is analogous to `Result<T, E>`: a tool either produces output
/// (the normal case) or requests a pause for external input (the interrupt case).
/// Both variants are expected and handled by the agent loop — neither is an error.
///
/// # Example
///
/// ```ignore
/// use remi_agentloop_core::tool::{ToolOutput, ToolResult};
/// use futures::stream;
///
/// // Normal result — final text
/// ToolResult::Output(stream::once(async { ToolOutput::text("done") }))
///
/// // Interrupt — wait for human approval
/// ToolResult::Interrupt(InterruptRequest::new("human_approval", serde_json::json!({})))
/// ```
pub enum ToolResult<S> {
    /// Normal execution — a stream of [`ToolOutput`] items.
    Output(S),
    /// Interrupt — pause the loop and wait for the caller to provide
    /// a [`ResumePayload`](crate::types::ResumePayload).
    Interrupt(InterruptRequest),
}

impl<S> ToolResult<S> {
    /// Returns `true` if this is the `Output` variant.
    pub fn is_output(&self) -> bool {
        matches!(self, Self::Output(_))
    }
    /// Returns `true` if this is the `Interrupt` variant.
    pub fn is_interrupt(&self) -> bool {
        matches!(self, Self::Interrupt(_))
    }

    /// Map the inner stream without unwrapping — analogous to `Result::map`.
    pub fn map_stream<S2>(self, f: impl FnOnce(S) -> S2) -> ToolResult<S2> {
        match self {
            Self::Output(s) => ToolResult::Output(f(s)),
            Self::Interrupt(req) => ToolResult::Interrupt(req),
        }
    }
}

// ── Tool Trait ────────────────────────────────────────────────────────────────

/// A callable tool available to the agent's language model.
///
/// Implement this trait to expose any capability — web search, code execution,
/// file I/O, API calls, database queries — to the model.
///
/// The framework ships several built-in implementations:
/// - `BashTool` (feature `tool-bash`) — execute shell commands
/// - `FsTool` / `LocalFsTool` (feature `tool-fs`) — read/write files
/// - `VirtualBashTool` / `VirtualFsTool` — sandboxed in-process variants
///
/// Use the [`#[tool]`](macro@crate::tool_macro) attribute macro to generate
/// `Tool` implementations from ordinary async functions.
///
/// # Minimal hand-written implementation
///
/// ```ignore
/// use remi_agentloop_core::prelude::*;
/// use futures::stream;
///
/// struct AddTool;
///
/// impl Tool for AddTool {
///     fn name(&self) -> &str { "add" }
///     fn description(&self) -> &str { "Add two integers and return the sum." }
///     fn parameters_schema(&self) -> serde_json::Value {
///         serde_json::json!({
///             "type": "object",
///             "properties": {
///                 "a": { "type": "integer" },
///                 "b": { "type": "integer" }
///             },
///             "required": ["a", "b"]
///         })
///     }
///     async fn execute(
///         &self,
///         args: serde_json::Value,
///         _resume: Option<ResumePayload>,
///         _ctx: &ToolContext,
///     ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
///         let a = args["a"].as_i64().unwrap_or(0);
///         let b = args["b"].as_i64().unwrap_or(0);
///         Ok(ToolResult::Output(stream::once(async move {
///             ToolOutput::text(format!("{}", a + b))
///         })))
///     }
/// }
/// ```
///
/// # Using the `#[tool]` macro
///
/// ```ignore
/// use remi_agentloop_core::tool_macro as tool;
///
/// #[tool(description = "Add two integers and return the sum.")]
/// async fn add(a: i64, b: i64) -> String {
///     format!("{}", a + b)
/// }
/// ```
pub trait Tool {
    /// Unique name used to identify the tool in model requests.
    ///
    /// Must match the identifier used in the JSON schema.  Use lowercase
    /// with underscores (e.g. `"web_search"`, `"read_file"`).
    fn name(&self) -> &str;

    /// Human-readable description sent to the model.
    ///
    /// Write this as if instructing the model when and how to use the tool.
    /// The more precise this is, the better the model's tool-selection accuracy.
    fn description(&self) -> &str;

    /// Optional per-request guidance appended to the tool definition sent to
    /// the model.
    ///
    /// Most tools should keep the default `None`. Override this only when the
    /// tool needs dynamic, runtime-specific instructions that cannot live in
    /// the static description.
    fn extra_prompt(&self, _ctx: &ToolDefinitionContext) -> Option<String> {
        None
    }

    /// JSON Schema for the tool's `arguments` object.
    ///
    /// Must be a JSON Schema `object` with a `properties` key.  The model
    /// uses this schema when constructing tool call arguments.
    ///
    /// # Example
    ///
    /// ```ignore
    /// fn parameters_schema(&self) -> serde_json::Value {
    ///     serde_json::json!({
    ///         "type": "object",
    ///         "properties": {
    ///             "query": { "type": "string", "description": "Search query" }
    ///         },
    ///         "required": ["query"]
    ///     })
    /// }
    /// ```
    fn parameters_schema(&self) -> serde_json::Value;

    /// Whether this tool is currently available to the model.
    ///
    /// `user_state` is a snapshot of [`AgentState::user_state`](crate::state::AgentState::user_state).
    /// Return `false` to hide the tool from the model's tool list — useful for
    /// *progressive disclosure*: reveal tools only after earlier steps succeed.
    ///
    /// Defaults to `true` (always enabled).
    ///
    /// # Example — progressive disclosure
    ///
    /// ```ignore
    /// fn enabled(&self, user_state: &serde_json::Value) -> bool {
    ///     // Only show the "checkout" tool after the cart has been filled
    ///     user_state["cart_ready"].as_bool().unwrap_or(false)
    /// }
    /// ```
    fn enabled(&self, _user_state: &serde_json::Value) -> bool {
        true
    }

    /// Execute the tool.
    /// `resume` is `Some` when this call is resuming from a previous
    /// [`InterruptRequest`]. The tool should use the payload to complete
    /// the operation that was interrupted.
    ///
    /// `ctx` provides runtime context (config, thread_id, run_id, metadata).
    /// Tools that don't need context can simply ignore the parameter.
    async fn execute(
        &self,
        arguments: serde_json::Value,
        resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>;
}

// ── ToolDefinition ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_prompt: Option<String>,
}

// ── DynTool ───────────────────────────────────────────────────────────────────

pub type BoxedToolStream<'a> = Pin<Box<dyn Stream<Item = ToolOutput> + 'a>>;
pub type BoxedToolResult<'a> = ToolResult<BoxedToolStream<'a>>;

/// Object-safe version of Tool (framework-internal)
pub(crate) trait DynTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn extra_prompt(&self, ctx: &ToolDefinitionContext) -> Option<String>;
    fn parameters_schema(&self) -> serde_json::Value;
    fn enabled(&self, user_state: &serde_json::Value) -> bool;
    fn execute_boxed<'a>(
        &'a self,
        arguments: serde_json::Value,
        resume: Option<ResumePayload>,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<BoxedToolResult<'a>, AgentError>> + 'a>>;
}

impl<T: Tool + Send + Sync> DynTool for T {
    fn name(&self) -> &str {
        Tool::name(self)
    }
    fn description(&self) -> &str {
        Tool::description(self)
    }
    fn extra_prompt(&self, ctx: &ToolDefinitionContext) -> Option<String> {
        Tool::extra_prompt(self, ctx)
    }
    fn parameters_schema(&self) -> serde_json::Value {
        Tool::parameters_schema(self)
    }
    fn enabled(&self, user_state: &serde_json::Value) -> bool {
        Tool::enabled(self, user_state)
    }

    fn execute_boxed<'a>(
        &'a self,
        arguments: serde_json::Value,
        resume: Option<ResumePayload>,
        ctx: &'a ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<BoxedToolResult<'a>, AgentError>> + 'a>> {
        Box::pin(async move {
            let result = Tool::execute(self, arguments, resume, ctx).await?;
            Ok(result.map_stream(|s| -> BoxedToolStream<'_> { Box::pin(s) }))
        })
    }
}

/// Helper: convert Tool → ToolDefinition
pub(crate) fn tool_to_definition(
    tool: &dyn DynTool,
    ctx: &ToolDefinitionContext,
) -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".into(),
        function: FunctionDefinition {
            name: tool.name().into(),
            description: tool.description().into(),
            parameters: tool.parameters_schema(),
            extra_prompt: tool.extra_prompt(ctx),
        },
    }
}

pub mod registry;
