use async_stream::stream;
use futures::{Stream, StreamExt};
use std::future::Future;
use std::pin::Pin;

use crate::agent::Agent;
use crate::agent_loop::AgentLoop;
use crate::checkpoint::{CheckpointStatus, CheckpointStore, NoCheckpointStore};
use crate::config::AgentConfig;
use crate::context::{ContextStore, NoStore};
use crate::error::AgentError;
use crate::model::ChatModel;
use crate::state::{Action, AgentPhase};
use crate::tool::registry::{DefaultToolRegistry, ToolRegistry};
use crate::tool::Tool;
use crate::tracing::{DynTracer, ResumeTrace, Tracer};
use crate::types::{
    AgentEvent, ChatInput, ChatReplayCursor, ChatSessionBundle, Content, Message, MessageId, Role,
    ThreadId, ToolCallOutcome,
};

// ── Typestate markers ─────────────────────────────────────────────────────────

/// Typestate marker — no model has been set yet.
///
/// `AgentBuilder` starts in state `<NoModel, NoStore, NoCheckpointStore>`.
/// Calling `.model(m)` transitions it to `<M, NoStore, NoCheckpointStore>`.
pub struct NoModel;

// ── AgentBuilder ──────────────────────────────────────────────────────────────

/// Typestate builder for [`AgentLoop`] and [`BuiltAgent`].
///
/// Use [`AgentBuilder::new()`] to start a builder chain.  The type parameters
/// encode whether a model, context store, and checkpoint store have been set:
///
/// | Parameter | Unset | Set |
/// |-----------|-------|-----|
/// | `M`       | [`NoModel`] | any [`ChatModel`] |
/// | `S`       | [`NoStore`] | any [`ContextStore`] |
/// | `C`       | [`NoCheckpointStore`] | any [`CheckpointStore`] |
///
/// `.build()` and `.build_loop()` are only available once `M` is set
/// (the compiler enforces this at the type level).
///
/// # Minimal example
///
/// ```ignore
/// use remi_agentloop_core::prelude::*;
///
/// let agent = AgentBuilder::new()
///     .model(oai_client)            // required — sets M
///     .system("You are helpful.")
///     .tool(WebSearchTool::new())
///     .max_turns(20)
///     .build();                     // → BuiltAgent (stateless, no memory)
/// ```
///
/// # With memory (context store)
///
/// ```ignore
/// let agent = AgentBuilder::new()
///     .model(oai_client)
///     .context_store(InMemoryStore::new())
///     .checkpoint_store(InMemoryCheckpointStore::new())
///     .system("You are a coding assistant.")
///     .build();                     // → BuiltAgent<M, InMemoryStore, InMemoryCheckpointStore>
///
/// let thread = agent.create_thread().await?;
/// let stream = agent.chat_in_thread(&thread, "write bubble sort").await?;
/// ```
pub struct AgentBuilder<M = NoModel, S = NoStore, C = NoCheckpointStore> {
    pub(crate) model: M,
    pub(crate) store: S,
    pub(crate) checkpoint_store: C,
    pub(crate) config: AgentConfig,
    pub(crate) tracer: Option<Box<dyn DynTracer>>,
    pub(crate) system_prompt: Option<String>,
    pub(crate) tools: DefaultToolRegistry,
    pub(crate) max_turns: usize,
}

impl AgentBuilder<NoModel, NoStore, NoCheckpointStore> {
    /// Create a new builder with default settings.
    ///
    /// Defaults: `max_turns = 10`, no system prompt, no tools, no stores.
    pub fn new() -> Self {
        AgentBuilder {
            model: NoModel,
            store: NoStore,
            checkpoint_store: NoCheckpointStore,
            config: AgentConfig::default(),
            tracer: None,
            system_prompt: None,
            tools: DefaultToolRegistry::new(),
            max_turns: 10,
        }
    }
}

impl<M, S, C> AgentBuilder<M, S, C> {
    /// Set the language model — required to call `.build()` or `.build_loop()`.
    ///
    /// Transitions the `M` typestate from [`NoModel`] to the concrete model type.
    /// Any [`ChatModel`] implementation is accepted, including [`OpenAIClient`](remi_model::OpenAIClient)
    /// and custom wrappers.
    ///
    /// ```ignore
    /// let builder = AgentBuilder::new().model(OpenAIClient::from_env());
    /// ```
    pub fn model<NewM: ChatModel>(self, model: NewM) -> AgentBuilder<NewM, S, C> {
        AgentBuilder {
            model,
            store: self.store,
            checkpoint_store: self.checkpoint_store,
            config: self.config,
            tracer: self.tracer,
            system_prompt: self.system_prompt,
            tools: self.tools,
            max_turns: self.max_turns,
        }
    }

    /// Attach a context store for persistent conversation memory.
    ///
    /// When set, the built [`BuiltAgent`] operates in *stateful mode*,
    /// enabling [`chat_in_thread`](BuiltAgent::chat_in_thread) and automatic
    /// message persistence.
    ///
    /// ```ignore
    /// let agent = AgentBuilder::new()
    ///     .model(oai)
    ///     .context_store(InMemoryStore::new())
    ///     .build();
    /// ```
    pub fn context_store<NewS: ContextStore>(self, store: NewS) -> AgentBuilder<M, NewS, C> {
        AgentBuilder {
            model: self.model,
            store,
            checkpoint_store: self.checkpoint_store,
            config: self.config,
            tracer: self.tracer,
            system_prompt: self.system_prompt,
            tools: self.tools,
            max_turns: self.max_turns,
        }
    }

    /// Set the checkpoint store — enables durable state snapshots for resume.
    pub fn checkpoint_store<NewC: CheckpointStore>(self, cs: NewC) -> AgentBuilder<M, S, NewC> {
        AgentBuilder {
            model: self.model,
            store: self.store,
            checkpoint_store: cs,
            config: self.config,
            tracer: self.tracer,
            system_prompt: self.system_prompt,
            tools: self.tools,
            max_turns: self.max_turns,
        }
    }

    /// Replace the default in-process registry with a custom [`ToolRegistry`] implementation.
    ///
    /// Note: any tools previously registered via [`.tool()`](Self::tool) will be discarded.
    pub fn with_registry(
        self,
        registry: impl ToolRegistry + 'static,
    ) -> AgentBuilderWithRegistry<M, S, C> {
        AgentBuilderWithRegistry {
            inner: AgentBuilder {
                model: self.model,
                store: self.store,
                checkpoint_store: self.checkpoint_store,
                config: self.config,
                tracer: self.tracer,
                system_prompt: self.system_prompt,
                tools: DefaultToolRegistry::new(), // unused
                max_turns: self.max_turns,
            },
            registry: Box::new(registry),
        }
    }

    /// Set the system prompt sent at the start of every conversation.
    ///
    /// ```ignore
    /// .system("You are a senior Rust engineer. Be concise and precise.")
    /// ```
    pub fn system(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Register a tool available to the model during this agent's runs.
    ///
    /// Can be called multiple times to add multiple tools.  Tools are evaluated
    /// in registration order; registering a tool with a duplicate name overwrites
    /// the previous one.
    ///
    /// ```ignore
    /// .tool(WebSearchTool::new())
    /// .tool(CodeExecutorTool::new())
    /// ```
    pub fn tool(mut self, tool: impl Tool + Send + Sync + 'static) -> Self {
        self.tools.register(tool);
        self
    }

    /// Set the maximum number of model turns per run (default: 10).
    ///
    /// If the loop hasn't finished by turn `n`, it returns
    /// [`AgentError::MaxTurnsExceeded`](crate::error::AgentError::MaxTurnsExceeded).
    pub fn max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }

    /// Override the runtime configuration.
    ///
    /// The [`AgentConfig`] holds API key, model name, base URL, temperature,
    /// and other per-request options.  Use this as an alternative to setting
    /// each field individually.
    ///
    /// ```ignore
    /// .config(AgentConfig::from_env())
    /// // or
    /// .config(AgentConfig::new().with_model("gpt-4o").with_temperature(0.3))
    /// ```
    pub fn config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    /// Attach an observability tracer.
    ///
    /// The tracer receives structured notifications at each lifecycle boundary
    /// (run start/end, model call, tool call, interrupt, resume).
    ///
    /// ```ignore
    /// .tracer(StdoutTracer::new())  // prints to stdout
    /// // or
    /// .tracer(LangSmithTracer::new(api_key, project))  // sends to LangSmith
    /// ```
    pub fn tracer(mut self, tracer: impl Tracer + Send + Sync + 'static) -> Self {
        self.tracer = Some(Box::new(tracer));
        self
    }

    /// Set provider-specific extra parameters included in every chat request body.
    ///
    /// The entries are flattened into the top-level JSON object sent to the model,
    /// enabling any OpenAI-compatible field not directly modelled by [`ChatRequest`]
    /// (e.g. `top_p`, `presence_penalty`, provider extensions).
    ///
    /// ```ignore
    /// use serde_json::json;
    ///
    /// let agent = AgentBuilder::new()
    ///     .model(oai)
    ///     .extra_options(
    ///         json!({ "top_p": 0.9, "presence_penalty": 0.1 })
    ///             .as_object().unwrap().clone()
    ///     )
    ///     .build();
    /// ```
    pub fn extra_options(mut self, options: serde_json::Map<String, serde_json::Value>) -> Self {
        self.config.extra = serde_json::Value::Object(options);
        self
    }
}

impl<M: ChatModel, S, C> AgentBuilder<M, S, C> {
    /// Build into just the [`AgentLoop`] (no store / memory layer).
    ///
    /// Use this when you need composable external tool calling:
    /// ```ignore
    /// let inner = AgentBuilder::new()
    ///     .model(oai)
    ///     .tool(Add::new())
    ///     .build_loop();
    ///
    /// let mut state = inner.build_state(vec![]);
    /// state.tool_definitions.extend(outer_defs);
    /// let stream = inner.run(state, action, true);
    /// ```
    pub fn build_loop(self) -> AgentLoop<M> {
        AgentLoop {
            model: self.model,
            tools: Box::new(self.tools),
            config: self.config,
            tracer: self.tracer,
            system_prompt: self.system_prompt.unwrap_or_default(),
            max_turns: self.max_turns,
        }
    }

    /// Build into a [`BuiltAgent`] — the full-featured agent with memory and lifecycle.
    ///
    /// Only available once a model has been set via [`.model()`](AgentBuilder::model).
    ///
    /// ```ignore
    /// let agent = AgentBuilder::new()
    ///     .model(oai)
    ///     .system("You are helpful.")
    ///     .context_store(InMemoryStore::new())
    ///     .build();
    /// ```
    pub fn build(self) -> BuiltAgent<M, S, C> {
        let system_prompt = self.system_prompt.unwrap_or_default();
        let inner = AgentLoop {
            model: self.model,
            tools: Box::new(self.tools),
            config: self.config,
            tracer: self.tracer,
            system_prompt: system_prompt.clone(),
            max_turns: self.max_turns,
        };
        BuiltAgent {
            inner,
            store: self.store,
            checkpoint_store: self.checkpoint_store,
            system_prompt,
        }
    }
}

// ── AgentBuilderWithRegistry ──────────────────────────────────────────────────

/// A builder variant that holds a custom [`ToolRegistry`] implementation.
pub struct AgentBuilderWithRegistry<M = NoModel, S = NoStore, C = NoCheckpointStore> {
    pub(crate) inner: AgentBuilder<M, S, C>,
    pub(crate) registry: Box<dyn ToolRegistry>,
}

impl<M: ChatModel, S, C> AgentBuilderWithRegistry<M, S, C> {
    pub fn build(self) -> BuiltAgent<M, S, C> {
        let system_prompt = self.inner.system_prompt.unwrap_or_default();
        let inner = AgentLoop {
            model: self.inner.model,
            tools: self.registry,
            config: self.inner.config,
            tracer: self.inner.tracer,
            system_prompt: system_prompt.clone(),
            max_turns: self.inner.max_turns,
        };
        BuiltAgent {
            inner,
            store: self.inner.store,
            checkpoint_store: self.inner.checkpoint_store,
            system_prompt,
        }
    }
}

// ── BuiltAgent ────────────────────────────────────────────────────────────────

/// Agent with optional memory persistence and checkpoint support.
///
/// Wraps an [`AgentLoop`] to add context-store persistence, checkpoint
/// persistence, and run lifecycle:
///
/// ```text
/// BuiltAgent (memory + checkpoints + run lifecycle)
///   └── AgentLoop (step + tools + tracing)  ← composable core
///         └── step()
/// ```
///
/// The inner `AgentLoop` emits [`AgentEvent::Checkpoint`] at key lifecycle
/// boundaries.  `BuiltAgent` intercepts these, persists messages to the
/// [`ContextStore`] and snapshots to the [`CheckpointStore`], then filters
/// them out of the stream forwarded to the caller.
pub struct BuiltAgent<M: ChatModel, S = NoStore, C = NoCheckpointStore> {
    pub(crate) inner: AgentLoop<M>,
    pub(crate) store: S,
    pub(crate) checkpoint_store: C,
    pub(crate) system_prompt: String,
}

impl<M: ChatModel, S: ContextStore, C: CheckpointStore> BuiltAgent<M, S, C> {
    /// Thin wrapper around [`AgentLoop::run`] that adds memory + checkpoint persistence.
    ///
    /// Observes the inner stream:
    /// - `Checkpoint(cp)` → persists messages to context store + checkpoint to
    ///   checkpoint store, then swallows the event
    /// - `Done` / `Interrupt` → marks run as complete in store
    /// - everything else → forwarded as-is
    fn run_loop<'a>(
        &'a self,
        state: crate::state::AgentState,
        action: Action,
        emit_run_start: bool,
    ) -> impl Stream<Item = AgentEvent> + 'a {
        let thread_id = state.thread_id.clone();
        let run_id = state.run_id.clone();
        let inner_stream = self.inner.run(state, action, emit_run_start);

        stream! {
            let mut known_msg_count = 0usize;
            let mut inner_stream = std::pin::pin!(inner_stream);
            while let Some(event) = inner_stream.next().await {
                match event {
                    AgentEvent::Checkpoint(cp) => {
                        // Persist new messages to context store
                        if cp.state.messages.len() > known_msg_count {
                            let new_msgs = cp.state.messages[known_msg_count..].to_vec();
                            let _ = self.store.append_messages(&thread_id, new_msgs).await;
                            known_msg_count = cp.state.messages.len();
                        }
                        // Persist checkpoint — swallow event
                        let _ = self.checkpoint_store.save(cp).await;
                    }
                    AgentEvent::Done => {
                        let _ = self.store.complete_run(&run_id).await;
                        yield AgentEvent::Done;
                    }
                    AgentEvent::Interrupt { interrupts } => {
                        let _ = self.store.complete_run(&run_id).await;
                        yield AgentEvent::Interrupt { interrupts };
                    }
                    other => {
                        yield other;
                    }
                }
            }
        }
    }

    /// Cancel a run and produce a `Cancelled` checkpoint.
    ///
    /// Wraps the inner `AgentLoop::cancel()` with persistence:
    /// persists checkpoint + context, yields `Cancelled`.
    ///
    /// If `partial_response` is `Some`, an incomplete assistant message is
    /// appended to `state.messages` before the checkpoint is saved, so the
    /// partial text survives a process restart.
    fn cancel_loop<'a>(
        &'a self,
        mut state: crate::state::AgentState,
        partial_response: Option<String>,
    ) -> impl Stream<Item = AgentEvent> + 'a {
        // Inject the partial assistant message before handing off to cancel_run.
        if let Some(text) = partial_response {
            if !text.is_empty() {
                use crate::types::{Content, Message, Role};
                state.messages.push(Message {
                    id: crate::types::MessageId::new(),
                    role: Role::Assistant,
                    content: Content::text(text),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                    reasoning_content: None,
                    metadata: None,
                });
            }
        }
        let thread_id = state.thread_id.clone();
        let run_id = state.run_id.clone();
        let inner_stream = self.inner.cancel_run(state);

        stream! {
            let mut known_msg_count = 0usize;
            let mut inner_stream = std::pin::pin!(inner_stream);
            while let Some(event) = inner_stream.next().await {
                match event {
                    AgentEvent::Checkpoint(cp) => {
                        if cp.state.messages.len() > known_msg_count {
                            let new_msgs = cp.state.messages[known_msg_count..].to_vec();
                            let _ = self.store.append_messages(&thread_id, new_msgs).await;
                            known_msg_count = cp.state.messages.len();
                        }
                        let _ = self.checkpoint_store.save(cp).await;
                    }
                    AgentEvent::Cancelled => {
                        let _ = self.store.complete_run(&run_id).await;
                        yield AgentEvent::Cancelled;
                    }
                    other => {
                        yield other;
                    }
                }
            }
        }
    }

    /// Resume execution from the latest checkpoint for a given thread.
    ///
    /// Loads the most recent checkpoint, extracts the agent state and pending
    /// action, and resumes the agent loop from that point.
    ///
    /// Returns `None` if no checkpoint exists for the thread.
    pub async fn resume_from_checkpoint(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<impl Stream<Item = AgentEvent> + '_>, AgentError> {
        let cp = self
            .checkpoint_store
            .load_latest_by_thread(thread_id)
            .await?;
        match cp {
            None => Ok(None),
            Some(cp) => {
                if !cp.is_resumable() {
                    // Terminal checkpoint — nothing to resume
                    return Ok(None);
                }
                let action = cp.pending_action.unwrap();
                let payloads_count = match &action {
                    crate::state::Action::ToolResults(r) => r.len(),
                    _ => 0,
                };
                if let Some(t) = self.inner.tracer.as_deref() {
                    t.on_resume(&ResumeTrace {
                        run_id: cp.state.run_id.clone(),
                        payloads_count,
                        outcomes: Vec::new(),
                        timestamp: chrono::Utc::now(),
                    })
                    .await;
                }
                Ok(Some(self.run_loop(cp.state, action, false)))
            }
        }
    }

    /// Export the latest checkpoint-backed session bundle for a thread.
    pub async fn export_thread_bundle(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Option<ChatSessionBundle>, AgentError> {
        let checkpoint = self
            .checkpoint_store
            .load_latest_by_thread(thread_id)
            .await?;
        let Some(checkpoint) = checkpoint else {
            return Ok(None);
        };

        let checkpoints = self
            .checkpoint_store
            .list_by_run(&checkpoint.run_id)
            .await?;

        Ok(Some(
            ChatSessionBundle::new(checkpoint.state).with_checkpoints(checkpoints),
        ))
    }

    /// Start a replay from a specific historical message on a forked thread.
    ///
    /// This creates a new thread containing copied history up to the selected
    /// message, loads the latest replay-safe checkpoint state for the source
    /// thread, truncates the state history, and starts a fresh run.
    pub async fn replay_from_message(
        &self,
        thread_id: &ThreadId,
        up_to_message: &MessageId,
    ) -> Result<Option<(ThreadId, Pin<Box<dyn Stream<Item = AgentEvent> + '_>>)>, AgentError> {
        let messages = self.store.get_messages(thread_id).await?;
        let replay_index = messages
            .iter()
            .position(|message| message.id == *up_to_message)
            .ok_or_else(|| AgentError::MessageNotFound(up_to_message.clone()))?;
        self.replay_from_message_index(thread_id, replay_index)
            .await
    }

    /// Start a replay from a specific historical message index on a forked thread.
    pub async fn replay_from_message_index(
        &self,
        thread_id: &ThreadId,
        replay_index: usize,
    ) -> Result<Option<(ThreadId, Pin<Box<dyn Stream<Item = AgentEvent> + '_>>)>, AgentError> {
        let checkpoint = self
            .checkpoint_store
            .load_latest_by_thread(thread_id)
            .await?;
        let Some(checkpoint) = checkpoint else {
            return Ok(None);
        };
        if !checkpoint.is_replayable() {
            return Err(AgentError::ReplayFromCheckpointNotAllowed {
                status: checkpoint_status_name(&checkpoint.status).to_string(),
            });
        }

        let source_run_id = checkpoint.run_id.clone();
        let source_messages = self.store.get_messages(thread_id).await?;
        if replay_index >= source_messages.len() {
            return Err(AgentError::ReplayIndexOutOfBounds {
                thread_id: thread_id.clone(),
                requested: replay_index,
                available: source_messages.len(),
            });
        }

        let new_thread = self.store.create_thread().await?;
        let mut replay_state = checkpoint.state;
        replay_state.messages = source_messages[..=replay_index]
            .iter()
            .cloned()
            .map(|message| Message {
                id: MessageId::new(),
                ..message
            })
            .collect();
        replay_state.thread_id = new_thread.clone();
        replay_state.run_id = self.store.create_run(&new_thread).await?;
        replay_state.turn = 0;
        replay_state.model_call_seq = 0;
        replay_state.phase = AgentPhase::Ready;
        replay_state.system_prompt = None;

        let cursor = ChatReplayCursor {
            start_message_index: replay_index,
            start_message_id: replay_state
                .messages
                .last()
                .map(|message| message.id.clone()),
        };
        annotate_replay_state(&mut replay_state, thread_id, &source_run_id, cursor);

        Ok(Some((
            new_thread,
            Box::pin(self.run_loop(replay_state, Action::ToolResults(vec![]), false)),
        )))
    }

    /// Flush any buffered tracing I/O (e.g. pending LangSmith HTTP calls).
    /// Call this after the agent event stream has been fully consumed.
    pub async fn flush_tracer(&self) {
        self.inner.flush_tracer().await;
    }
}

fn checkpoint_status_name(status: &CheckpointStatus) -> &'static str {
    match status {
        CheckpointStatus::StepDone => "step_done",
        CheckpointStatus::AwaitingToolExecution => "awaiting_tool_execution",
        CheckpointStatus::ToolsExecuted => "tools_executed",
        CheckpointStatus::RunDone => "run_done",
        CheckpointStatus::Interrupted => "interrupted",
        CheckpointStatus::Errored => "errored",
        CheckpointStatus::Cancelled => "cancelled",
    }
}

fn annotate_replay_state(
    state: &mut crate::state::AgentState,
    source_thread_id: &ThreadId,
    source_run_id: &crate::types::RunId,
    cursor: ChatReplayCursor,
) {
    let mut user_state = match state.user_state.take() {
        serde_json::Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            if !other.is_null() {
                map.insert("previous_user_state".into(), other);
            }
            map
        }
    };

    user_state.insert(
        "replay".into(),
        serde_json::json!({
            "source_thread_id": source_thread_id,
            "source_run_id": source_run_id,
            "start_message_index": cursor.start_message_index,
            "start_message_id": cursor.start_message_id,
        }),
    );
    state.user_state = serde_json::Value::Object(user_state);
}

// ── Agent impl (stateless) ────────────────────────────────────────────────────

impl<M: ChatModel> Agent for BuiltAgent<M, NoStore, NoCheckpointStore> {
    type Request = crate::types::LoopInput;
    type Response = AgentEvent;
    type Error = AgentError;

    /// Stateless mode: delegates to the inner [`AgentLoop`].
    fn chat(
        &self,
        input: crate::types::LoopInput,
    ) -> impl Future<Output = Result<impl Stream<Item = AgentEvent>, AgentError>> {
        self.inner.chat(input)
    }
}

// ── Stateful mode ─────────────────────────────────────────────────────────────

impl<M: ChatModel, S: ContextStore, C: CheckpointStore> BuiltAgent<M, S, C> {
    /// Create a new conversation thread in the context store.
    ///
    /// Returns a [`ThreadId`] that can be passed to [`chat_in_thread`](Self::chat_in_thread).
    ///
    /// ```ignore
    /// let tid = agent.create_thread().await?;
    /// let stream = agent.chat_in_thread(&tid, "Hello!").await?;
    /// ```
    pub async fn create_thread(&self) -> Result<ThreadId, AgentError> {
        self.store.create_thread().await
    }

    /// Send a message (or resume from an interrupt) within an existing thread.
    ///
    /// This is the primary entry point for stateful conversations.  It
    /// loads prior messages from the context store, appends the new message,
    /// runs the agent loop, and persists results — all transparently.
    ///
    /// # Sending a new user message
    ///
    /// ```ignore
    /// let tid = agent.create_thread().await?;
    /// let mut stream = agent.chat_in_thread(&tid, "What is Rust?").await?;
    /// while let Some(ev) = stream.next().await {
    ///     if let AgentEvent::TextDelta(s) = ev { print!("{s}"); }
    /// }
    /// ```
    ///
    /// # Resuming after an interrupt
    ///
    /// ```ignore
    /// // The previous run ended with AgentEvent::Interrupt { interrupts }
    /// let mut stream = agent.chat_in_thread(
    ///     &tid,
    ///     ChatInput::Resume {
    ///         run_id,
    ///         completed_results: vec![],
    ///         pending_interrupts: interrupts,
    ///         payloads: vec![ResumePayload {
    ///             interrupt_id: interrupts[0].interrupt_id.clone(),
    ///             result: serde_json::json!({ "approved": true }),
    ///         }],
    ///     },
    /// ).await?;
    /// ```
    pub async fn chat_in_thread(
        &self,
        thread_id: &ThreadId,
        input: impl Into<ChatInput>,
    ) -> Result<Pin<Box<dyn Stream<Item = AgentEvent> + '_>>, AgentError> {
        let input = input.into();

        // ── Load existing messages from store ─────────────────────────
        let mut messages = self.store.get_messages(thread_id).await?;

        // Ensure system prompt is first
        if !self.system_prompt.is_empty() {
            if !messages
                .first()
                .is_some_and(|m| matches!(m.role, Role::System))
            {
                messages.insert(0, Message::system(&self.system_prompt));
            }
        }

        match input {
            ChatInput::Message {
                content: user_content,
                user_name,
            } => {
                let run_id = self.store.create_run(thread_id).await?;

                // Append user message to store + state
                let user_msg = Message {
                    id: crate::types::MessageId::new(),
                    role: Role::User,
                    content: user_content,
                    tool_calls: None,
                    tool_call_id: None,
                    name: user_name,
                    reasoning_content: None,
                    metadata: None,
                };
                self.store
                    .append_message(thread_id, user_msg.clone())
                    .await?;
                messages.push(user_msg);

                let mut state = self.inner.build_state(messages);
                state.thread_id = thread_id.clone();
                state.run_id = run_id;
                state.system_prompt = None;

                // Action is empty ToolResults because user message is already in state.messages;
                // step() will see it and call the model.
                Ok(Box::pin(self.run_loop(
                    state,
                    Action::ToolResults(vec![]),
                    false,
                )))
            }

            ChatInput::Resume {
                run_id,
                completed_results,
                pending_interrupts,
                payloads,
            } => {
                // ── Validate payloads match pending interrupts ─────────
                if payloads.len() != pending_interrupts.len() {
                    return Err(AgentError::ResumeIncomplete {
                        expected: pending_interrupts.len(),
                        got: payloads.len(),
                    });
                }
                for intr in &pending_interrupts {
                    if !payloads.iter().any(|p| p.interrupt_id == intr.interrupt_id) {
                        return Err(AgentError::InterruptNotFound(intr.interrupt_id.clone()));
                    }
                }

                // ── Build tool outcomes (don't add messages manually —
                //    step() will do that via Action::ToolResults, and
                //    run_loop will persist them automatically) ──────────
                let mut outcomes: Vec<ToolCallOutcome> = Vec::new();

                for tr in &completed_results {
                    outcomes.push(ToolCallOutcome::Result {
                        tool_call_id: tr.id.clone(),
                        tool_name: tr.name.clone(),
                        content: Content::text(tr.result.clone()),
                    });
                }

                for payload in &payloads {
                    let intr = pending_interrupts
                        .iter()
                        .find(|i| i.interrupt_id == payload.interrupt_id)
                        .unwrap();
                    let result_str = serde_json::to_string(&payload.result).unwrap_or_default();
                    outcomes.push(ToolCallOutcome::Result {
                        tool_call_id: intr.tool_call_id.clone(),
                        tool_name: intr.tool_name.clone(),
                        content: Content::text(result_str),
                    });
                }

                let mut state = self.inner.build_state(messages);
                state.thread_id = thread_id.clone();
                state.run_id = run_id;
                state.system_prompt = None;

                // step() will append tool_result messages to state.messages,
                // run_loop will detect & persist the new messages automatically.
                let payloads_count = outcomes.len();
                if let Some(t) = self.inner.tracer.as_deref() {
                    t.on_resume(&ResumeTrace {
                        run_id: state.run_id.clone(),
                        payloads_count,
                        outcomes: outcomes
                            .iter()
                            .map(|outcome| match outcome {
                                ToolCallOutcome::Result {
                                    tool_call_id,
                                    tool_name,
                                    content,
                                } => crate::tracing::ToolOutcomeTrace {
                                    tool_call_id: tool_call_id.clone(),
                                    tool_name: tool_name.clone(),
                                    result: Some(content.text_content()),
                                    error: None,
                                },
                                ToolCallOutcome::Error {
                                    tool_call_id,
                                    tool_name,
                                    error,
                                } => crate::tracing::ToolOutcomeTrace {
                                    tool_call_id: tool_call_id.clone(),
                                    tool_name: tool_name.clone(),
                                    result: None,
                                    error: Some(error.clone()),
                                },
                            })
                            .collect(),
                        timestamp: chrono::Utc::now(),
                    })
                    .await;
                }
                Ok(Box::pin(self.run_loop(
                    state,
                    Action::ToolResults(outcomes),
                    false,
                )))
            }

            ChatInput::Cancel {
                run_id,
                partial_response,
            } => {
                // Load the latest checkpoint for this run to get the current state
                let cp = self.checkpoint_store.load_latest_by_run(&run_id).await?;
                let state = match cp {
                    Some(cp) => cp.state,
                    None => {
                        // No checkpoint yet — build state from stored messages
                        let mut state = self.inner.build_state(messages);
                        state.thread_id = thread_id.clone();
                        state.run_id = run_id;
                        state.system_prompt = None;
                        state
                    }
                };
                Ok(Box::pin(self.cancel_loop(state, partial_response)))
            }
        }
    }
}

impl Default for AgentBuilder<NoModel, NoStore, NoCheckpointStore> {
    fn default() -> Self {
        Self::new()
    }
}
