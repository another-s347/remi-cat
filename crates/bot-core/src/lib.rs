//! `bot-core` — DeepAgent re-implementation for remi-cat.
//!
//! ## Architecture
//!
//! ```text
//! CatBot
//!   └── CatAgent<AgentLoop<OpenAIClient>>
//!         ├── local_tools: search / skill__get / skill__read_resource
//!         ├── local_tools: todo__add / todo__list / todo__complete / todo__update / todo__remove
//!         └── local_tools: memory__get_detail
//!   └── MemoryStore  (shared via Arc)
//!         ├── .remi-cat/Agent.md + Soul.md  (injected as System messages every turn)
//!         ├── short_term.jsonl              (raw recent messages, per thread)
//!         ├── mid_term/<uuid>.md            (LLM-compressed, with raw archive)
//!         └── long_term/<uuid>.md           (LLM-compressed, with raw archive)
//! ```

pub mod acp;
pub mod agent;
pub mod events;
pub mod goal;
pub mod im_tools;
pub mod memory;
pub mod model_profile;
pub mod profile;
pub mod sandbox;
pub mod search;
pub mod skill;
pub mod todo;
pub mod tools;
pub mod trigger;

pub use agent::CatAgent;
pub use events::{CatEvent, SkillEvent, TodoEvent, TriggerEvent};
pub use goal::{GoalMaxRounds, GoalState, GoalStatus, SupervisorDecision, SupervisorReport};
pub use im_tools::{ImAttachment, ImDocument, ImFileBridge};
pub use memory::MemoryStore;
pub use model_profile::{
    install_embedded_model_profiles, resolve_model_profile_from_env, ModelProfileConfig,
    ModelProfileRegistry, ModelProfileSource, ThinkingMode,
};
pub use profile::{
    install_embedded_agent_profiles, AgentModelBindings, AgentProfile, AgentRegistry,
};
pub use remi_agentloop::prelude::{Content, ContentPart, Message};
pub use skill::store::{BuiltinSkillStore, FileSkillStore};
pub use tools::SharedRedactor;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_agentloop::agent_loop::AgentLoop;
use remi_agentloop::prelude::{
    Agent, AgentBuilder, AgentConfig, AgentError, AgentEvent, LoopInput, OpenAIClient,
    ReqwestTransport, ToolContext,
};
use remi_agentloop::tool::registry::DefaultToolRegistry;
use remi_agentloop_deepagent::{SubAgentEventStream, SubAgentToolAdapter};
use tokio::sync::Mutex as AsyncMutex;

use crate::im_tools::register_fetch_tool;
use memory::{
    build_injected_history, LlmCompressor, MemoryGetDetailTool, MemoryRecallTool,
    MemoryUpsertNamedTool,
};
use sandbox::SandboxConfig;
use search::SearchTool;
use tools::{
    BashMode, ExaSearchTool, NowTool, RootedFsApplyPatchTool, RootedFsCreateTool, RootedFsLsTool,
    RootedFsReadTool, RootedFsRemoveTool, RootedFsWriteTool, SecretRedactor, SleepTool,
    WorkspaceBashTool,
};

const DEFAULT_AGENT_ID: &str = "default";
pub(crate) const TRIGGER_RUN_META_KEY: &str = "trigger_run";

pub(crate) fn metadata_flag_enabled(metadata: Option<&serde_json::Value>, key: &str) -> bool {
    metadata
        .and_then(|value| value.get(key))
        .map(|value| {
            value.as_bool().unwrap_or_else(|| {
                value
                    .as_str()
                    .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub(crate) fn suppress_trigger_management(metadata: Option<&serde_json::Value>) -> bool {
    metadata_flag_enabled(metadata, TRIGGER_RUN_META_KEY)
}

// -- StreamOptions ----------------------------------------------------------

/// Per-turn options for [`CatBot::stream_with_options`].
#[derive(Debug, Default, Clone)]
pub struct StreamOptions {
    /// UUID of the sender (stored in metadata; injected as a
    /// system annotation in group chats so the LLM can distinguish speakers).
    pub sender_user_id: Option<String>,
    /// IM username used for `Message.name` on the current user turn.
    pub sender_username: Option<String>,
    /// Feishu `message_id` of the incoming message (stored in metadata).
    pub message_id: Option<String>,
    /// Feishu `chat_type` — `"group"` or `"p2p"` (stored in metadata;
    /// triggers speaker annotation when `"group"`).
    pub chat_type: Option<String>,
    /// Current IM platform identifier (for example `feishu`).
    pub platform: Option<String>,
    /// Route newly-created todo batches to the remi-sdk backend when true.
    pub todo_create_via_sdk: bool,
    /// Enable trigger management tools for the current turn.
    pub trigger_tools_enabled: bool,
    /// Marks the request as an automatic trigger execution.
    /// Trigger management tools and the builtin trigger skill are hidden for these runs.
    pub trigger_run: bool,
    /// Marks the request as an internal supervisor run.
    /// Goal supervision is disabled for these runs to avoid recursive loops.
    pub supervisor_run: bool,
    /// Downloadable IM attachments referenced by the current message.
    pub im_attachments: Vec<ImAttachment>,
    /// Feishu document links referenced by the current message.
    pub im_documents: Vec<ImDocument>,
    /// Optional cooperative-cancel signal.  When the wrapped [`Notify`] is
    /// signalled (via `notify_one()`), `stream_with_options` will persist any
    /// already-generated content and yield a final [`CatEvent::Done`] before
    /// returning — so memory is not lost on preemption.
    pub cancel: Option<std::sync::Arc<tokio::sync::Notify>>,
}

// -- Type aliases -------------------------------------------------------------

type InnerAgent = AgentLoop<OpenAIClient<ReqwestTransport>>;
type ThreadRunLock = Arc<AsyncMutex<()>>;
type ThreadRunLocks = Arc<AsyncMutex<HashMap<String, ThreadRunLock>>>;

struct LocalAcpAgentRunner {
    agent: CatAgent<InnerAgent>,
    memory: Arc<MemoryStore>,
    run_locks: ThreadRunLocks,
}

fn local_acp_thread_id(session_id: &str) -> String {
    format!("acp:{session_id}")
}

async fn thread_run_lock(locks: &ThreadRunLocks, thread_id: &str) -> ThreadRunLock {
    let mut locks = locks.lock().await;
    locks
        .entry(thread_id.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

impl acp::AcpLocalRunner for LocalAcpAgentRunner {
    fn run<'a>(
        &'a self,
        session_id: &'a str,
        message: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + 'a>> {
        Box::pin(async move {
            let thread_id = local_acp_thread_id(session_id);
            let run_lock = thread_run_lock(&self.run_locks, &thread_id).await;
            let _run_guard = run_lock.lock().await;
            let mut ctx = self.memory.load_context(&thread_id).await?;
            let mut history = build_injected_history(&ctx);
            append_thread_todo_system_prompt(&mut history, &ctx.user_state);
            let skip_count = history.len();
            let input = LoopInput::start(message)
                .history(history)
                .metadata(serde_json::json!({ "thread_id": &thread_id }))
                .user_state(std::mem::take(&mut ctx.user_state));
            let mut stream = std::pin::pin!(self.agent.stream_with_input(input));
            let mut text = String::new();
            let mut raw_history: Option<Vec<Message>> = None;
            let mut raw_user_state: Option<serde_json::Value> = None;
            while let Some(event) = stream.next().await {
                match event {
                    CatEvent::Text(delta) => text.push_str(&delta),
                    CatEvent::History(messages, user_state) => {
                        raw_history = Some(messages);
                        raw_user_state = Some(user_state);
                    }
                    CatEvent::StateUpdate(user_state) => {
                        raw_user_state = Some(user_state);
                    }
                    CatEvent::Done => break,
                    CatEvent::Error(err) => {
                        anyhow::bail!("local ACP agent failed for session {session_id}: {err}")
                    }
                    _ => {}
                }
            }
            persist_turn(
                &self.memory,
                &thread_id,
                raw_history,
                raw_user_state,
                skip_count,
            )
            .await;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                anyhow::bail!("local ACP agent produced no text for session {session_id}");
            }
            Ok(trimmed.to_string())
        })
    }
}

// -- CatBot -------------------------------------------------------------------

/// Main bot handle.  Build with [`CatBotBuilder`] or [`CatBot::from_env`].
pub struct CatBot {
    inner: CatAgent<InnerAgent>,
    memory: Arc<MemoryStore>,
    todo_backend: Arc<todo::HybridTodoBackend>,
    trigger_backend: Arc<trigger::TriggerBackend>,
    acp_backend: Arc<acp::AcpBackend>,
    run_locks: ThreadRunLocks,
    model_profile: ModelProfileConfig,
    /// Shared secret redactor — updated via `update_secret_redactor`.
    redactor: SharedRedactor,
}

impl CatBot {
    /// Convenience constructor — reads credentials from environment variables.
    ///
    /// | Variable                  | Description                                           |
    /// |---------------------------|-------------------------------------------------------|
    /// | `OPENAI_API_KEY`          | API key (required)                                    |
    /// | `REMI_MODEL_PROFILE`      | Selected model profile id (default: `default`)        |
    /// | `REMI_KIMI_THINKING`      | Optional thinking override for supported Kimi profiles |
    /// | `REMI_MEMORY_DAYS`        | Days before mid-term → long-term (default: 7)         |
    /// | `LANGSMITH_API_KEY`       | Enable LangSmith tracing (optional)                   |
    /// | `LANGSMITH_PROJECT`       | LangSmith project name (default: `remi-cat`)          |
    pub fn from_env() -> anyhow::Result<Self> {
        CatBotBuilder::from_env()?.build()
    }

    /// Immediately flush all short-term memory into a new mid-term block.
    ///
    /// Returns the number of messages compressed, or `0` if already empty.
    pub async fn compact_memory(
        &self,
        thread_id: &str,
    ) -> Result<usize, remi_agentloop::prelude::AgentError> {
        let run_lock = self.thread_run_lock(thread_id).await;
        let _run_guard = run_lock.lock().await;
        self.memory.compact_now(thread_id).await
    }

    /// Clear conversation history for one thread while preserving tool state.
    pub async fn clear_memory(
        &self,
        thread_id: &str,
    ) -> Result<(), remi_agentloop::prelude::AgentError> {
        let run_lock = self.thread_run_lock(thread_id).await;
        let _run_guard = run_lock.lock().await;
        self.memory.clear_thread(thread_id).await
    }

    pub async fn set_goal(
        &self,
        thread_id: &str,
        goal_text: &str,
        max_rounds: GoalMaxRounds,
    ) -> Result<GoalState, remi_agentloop::prelude::AgentError> {
        let goal = GoalState {
            goal: goal_text.trim().to_string(),
            status: GoalStatus::Active,
            max_rounds,
            last_evaluation: None,
            updated_at: chrono::Utc::now(),
        };
        goal::save_goal(&self.memory.data_dir, thread_id, &goal)
            .await
            .map_err(|err| AgentError::Io(err.to_string()))?;
        Ok(goal)
    }

    pub async fn goal_status(&self, thread_id: &str) -> Option<GoalState> {
        goal::load_goal(&self.memory.data_dir, thread_id).await
    }

    pub async fn clear_goal(
        &self,
        thread_id: &str,
    ) -> Result<(), remi_agentloop::prelude::AgentError> {
        goal::clear_goal(&self.memory.data_dir, thread_id)
            .await
            .map_err(|err| AgentError::Io(err.to_string()))
    }

    async fn thread_run_lock(&self, thread_id: &str) -> ThreadRunLock {
        thread_run_lock(&self.run_locks, thread_id).await
    }

    /// List triggers for the current thread without invoking the LLM.
    pub async fn trigger_list_command(
        &self,
        thread_id: &str,
        opts: StreamOptions,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        let ctx = trigger_command_tool_ctx(thread_id, &opts);
        let items = self.trigger_backend.list(&ctx).await?;
        if items.is_empty() {
            Ok("当前线程没有触发器。".to_string())
        } else {
            Ok(format!(
                "**当前线程的触发器：**\n\n{}",
                crate::trigger::tools::format_trigger_list(&items)
            ))
        }
    }

    /// Delete one trigger for the current thread without invoking the LLM.
    pub async fn trigger_delete_command(
        &self,
        thread_id: &str,
        id: u64,
        opts: StreamOptions,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        let ctx = trigger_command_tool_ctx(thread_id, &opts);
        let result = self.trigger_backend.delete(&ctx, id).await?;
        if result.starts_with("Removed trigger #") {
            Ok(format!("✅ 已删除触发器 #{id}。"))
        } else if result.contains("not found") {
            Ok(format!("❌ 触发器 #{id} 不存在。"))
        } else {
            Ok(result)
        }
    }

    pub async fn acp_bound_message(
        &self,
        session_id: &str,
        message: &str,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        self.acp_backend
            .continue_bound_session(session_id, message)
            .await
            .map_err(|err| remi_agentloop::prelude::AgentError::tool("acp__chat", err.to_string()))
    }

    pub async fn acp_binding_status(
        &self,
        platform: &str,
        channel_id: &str,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        self.acp_backend
            .binding_status_text(platform, channel_id)
            .await
            .map_err(|err| remi_agentloop::prelude::AgentError::tool("acp__chat", err.to_string()))
    }

    pub async fn acp_unbind_channel(
        &self,
        platform: &str,
        channel_id: &str,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        let removed = self
            .acp_backend
            .unbind_channel(platform, channel_id)
            .await
            .map_err(|err| {
                remi_agentloop::prelude::AgentError::tool("acp__chat", err.to_string())
            })?;
        if removed {
            Ok("ACP 绑定已解除。".to_string())
        } else {
            Ok("当前频道没有绑定 ACP session。".to_string())
        }
    }

    /// Create or update one trigger from a JSON payload without invoking the LLM.
    pub async fn trigger_upsert_command(
        &self,
        thread_id: &str,
        arguments_json: &str,
        opts: StreamOptions,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        let args = serde_json::from_str(arguments_json).map_err(|err| {
            remi_agentloop::prelude::AgentError::tool(
                "trigger",
                format!("invalid trigger JSON: {err}"),
            )
        })?;
        let request = crate::trigger::tools::parse_upsert_request(args)?;
        let ctx = trigger_command_tool_ctx(thread_id, &opts);
        let result = self.trigger_backend.upsert(&ctx, request).await?;
        let action = if result.operation == "updated" {
            "已更新"
        } else {
            "已创建"
        };
        Ok(format!(
            "✅ {action}触发器。\n\n{}",
            crate::trigger::tools::format_trigger_item(&result.item)
        ))
    }

    /// Rebuild the secret redactor from a new `key → value` map.
    ///
    /// Called by the agent session loop when a `SecretsSync` gRPC message arrives.
    pub fn update_secret_redactor(&self, entries: &std::collections::HashMap<String, String>) {
        let new_redactor = SecretRedactor::from_entries(entries);
        *self.redactor.write().unwrap() = new_redactor;
    }

    /// Return `(name, description)` pairs for every registered tool.
    ///
    /// Useful for the `/tools` slash command — lets users see what the agent
    /// can do without reading source code.
    pub fn tool_list(&self) -> Vec<(String, String)> {
        use remi_agentloop::tool::registry::ToolRegistry;
        self.inner
            .local_tools
            .definitions(&serde_json::Value::Null)
            .into_iter()
            .filter(|d| {
                self.inner
                    .tool_allowlist
                    .as_ref()
                    .map(|allowlist| allowlist.iter().any(|tool| tool == &d.function.name))
                    .unwrap_or(true)
            })
            .map(|d| (d.function.name, d.function.description))
            .collect()
    }

    async fn evaluate_goal_after_round(
        &self,
        thread_id: &str,
        history: &[Message],
        completed_continuations: u32,
    ) -> GoalRoundOutcome {
        let Some(mut goal_state) = goal::load_goal(&self.memory.data_dir, thread_id).await else {
            return GoalRoundOutcome::NoGoal;
        };

        if goal_state.status == GoalStatus::Completed {
            if let Some(decision) = goal_state.last_evaluation.clone() {
                return GoalRoundOutcome::Report(SupervisorReport {
                    goal: goal_state.goal,
                    decision,
                    round: completed_continuations,
                    max_rounds: goal_state.max_rounds,
                });
            }
            return GoalRoundOutcome::NoGoal;
        }

        let decision = match self
            .run_supervisor_once(thread_id, &goal_state.goal, history)
            .await
        {
            Ok(decision) => decision,
            Err(err) => SupervisorDecision {
                status: goal::SupervisorDecisionStatus::Continue,
                message: None,
                reason: format!("supervisor failed: {err}"),
            },
        };

        if decision.status == goal::SupervisorDecisionStatus::Completed {
            goal_state.status = GoalStatus::Completed;
        }
        goal_state.last_evaluation = Some(decision.clone());
        goal_state.updated_at = chrono::Utc::now();
        if let Err(err) = goal::save_goal(&self.memory.data_dir, thread_id, &goal_state).await {
            tracing::warn!(thread_id, error = %err, "failed to save goal state");
        }

        let report = SupervisorReport {
            goal: goal_state.goal.clone(),
            decision: decision.clone(),
            round: completed_continuations,
            max_rounds: goal_state.max_rounds.clone(),
        };

        if decision.status != goal::SupervisorDecisionStatus::Continue {
            return GoalRoundOutcome::Report(report);
        }
        if !goal_round_allows_continue(&goal_state.max_rounds, completed_continuations) {
            return GoalRoundOutcome::Report(report);
        }
        match decision.message {
            Some(message) => GoalRoundOutcome::Continue { report, message },
            None => GoalRoundOutcome::Report(report),
        }
    }

    async fn run_supervisor_once(
        &self,
        thread_id: &str,
        goal_text: &str,
        history: &[Message],
    ) -> Result<SupervisorDecision, AgentError> {
        let supervisor_thread_id = format!("supervisor:{thread_id}:{}", uuid::Uuid::new_v4());
        let prompt = goal::supervisor_prompt(goal_text, history);
        let input = LoopInput::start(prompt).metadata(serde_json::json!({
            "thread_id": supervisor_thread_id,
            "supervisor_run": "true",
        }));
        let mut stream = std::pin::pin!(self.inner.stream_with_input(input));
        let mut output = String::new();
        while let Some(event) = stream.next().await {
            match event {
                CatEvent::Text(delta) => output.push_str(&delta),
                CatEvent::Error(err) => return Err(err),
                CatEvent::Done => break,
                _ => {}
            }
        }
        goal::parse_supervisor_decision(&output).map_err(AgentError::other)
    }

    /// Stream events for one conversation turn (text input).
    ///
    /// `thread_id` scopes the memory (use the Feishu `chat_id`).
    pub fn stream<'a>(
        &'a self,
        thread_id: &'a str,
        text: impl Into<String>,
    ) -> impl Stream<Item = CatEvent> + 'a {
        self.stream_content(thread_id, Content::text(text.into()))
    }

    /// Stream events for one conversation turn (arbitrary content — text, images, etc.).
    pub fn stream_content<'a>(
        &'a self,
        thread_id: &'a str,
        content: Content,
    ) -> impl Stream<Item = CatEvent> + 'a {
        self.stream_with_options(thread_id, content, StreamOptions::default())
    }

    /// Stream events with per-turn metadata (sender identity, chat type, etc.).
    ///
    /// `sender_user_id` and `message_id` are stored as metadata on the user
    /// message so they persist in conversation history without polluting the
    /// message body or adding standalone messages.
    pub fn stream_with_options<'a>(
        &'a self,
        thread_id: &'a str,
        content: Content,
        opts: StreamOptions,
    ) -> impl Stream<Item = CatEvent> + 'a {
        let thread_id_owned = thread_id.to_string();
        stream! {
            let run_lock = self.thread_run_lock(&thread_id_owned).await;
            let _run_guard = run_lock.lock().await;
            let mut next_content = content;
            let mut supervisor_round: u32 = 0;
            let mut continuation_from_supervisor = false;

            'goal_loop: loop {
            // 1. Load memory context (triggers mid->long-term promotion if needed).
            let mut ctx = match self.memory.load_context(&thread_id_owned).await {
                Ok(c) => c,
                Err(e) => { yield CatEvent::Error(e); return; }
            };

            if let Err(err) = self
                .todo_backend
                .refresh_thread_user_state(
                    &thread_id_owned,
                    opts.sender_user_id.as_deref(),
                    &mut ctx.user_state,
                )
                .await
            {
                tracing::warn!(
                    thread_id = %thread_id_owned,
                    error = %err,
                    "failed to refresh sdk-backed todo state before turn"
                );
            }

            if let Err(err) = self
                .trigger_backend
                .refresh_thread_user_state(
                    &thread_id_owned,
                    opts.sender_user_id.as_deref(),
                    &mut ctx.user_state,
                )
                .await
            {
                tracing::warn!(
                    thread_id = %thread_id_owned,
                    error = %err,
                    "failed to refresh sdk-backed trigger state before turn"
                );
            }

            let mut round_opts = opts.clone();
            if continuation_from_supervisor {
                round_opts.sender_username = Some("supervisor".to_string());
                round_opts.sender_user_id = Some("supervisor".to_string());
                round_opts.message_id = None;
                round_opts.im_attachments.clear();
                round_opts.im_documents.clear();
            }

            let requested_user_name = round_opts
                .sender_username
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let injected_user_name = requested_user_name
                .as_deref()
                .and_then(|value| truncate_user_name(Some(value), 10));
            let single_chat_sender_prompt = single_chat_sender_system_prompt(
                round_opts.chat_type.as_deref(),
                requested_user_name.as_deref(),
                round_opts.sender_user_id.as_deref(),
            );

            // 2. Build injected history prefix; record its length to strip later.
            let mut history = build_injected_history(&ctx);
            let agent_header_count =
                usize::from(ctx.agent_md.is_some()) + usize::from(ctx.soul_md.is_some());
            insert_single_chat_sender_system_prompt(
                &mut history,
                agent_header_count,
                single_chat_sender_prompt.clone(),
            );
            append_thread_todo_system_prompt(&mut history, &ctx.user_state);
            let skip_count = history.len();

            // 3. Build request-level metadata (thread_id for tools);
            //    build per-message metadata (sender identity + message id).
            let mut meta = serde_json::json!({ "thread_id": &thread_id_owned });
            if let Some(ref ct) = round_opts.chat_type {
                meta["chat_type"] = serde_json::Value::String(ct.clone());
            }
            if let Some(ref platform) = round_opts.platform {
                meta["platform"] = serde_json::Value::String(platform.clone());
            }
            if round_opts.todo_create_via_sdk {
                meta["todo_create_via_sdk"] = serde_json::Value::String("true".to_string());
            }
            if round_opts.trigger_tools_enabled {
                meta["trigger_tools_enabled"] = serde_json::Value::String("true".to_string());
            }
            if round_opts.trigger_run {
                meta[TRIGGER_RUN_META_KEY] = serde_json::Value::String("true".to_string());
            }
            if round_opts.supervisor_run {
                meta["supervisor_run"] = serde_json::Value::String("true".to_string());
            }

            let mut msg_meta = serde_json::Map::new();
            if let Some(ref sid) = round_opts.sender_user_id {
                msg_meta.insert("sender_user_id".into(), serde_json::Value::String(sid.clone()));
                meta["sender_user_id"] = serde_json::Value::String(sid.clone());
            }
            if let Some(ref username) = round_opts.sender_username {
                let username = username.trim();
                if !username.is_empty() {
                    let username = username.to_string();
                    msg_meta.insert("sender_username".into(), serde_json::Value::String(username.clone()));
                    meta["sender_username"] = serde_json::Value::String(username);
                }
            }
            if let Some(ref mid) = round_opts.message_id {
                msg_meta.insert("message_id".into(), serde_json::Value::String(mid.clone()));
                meta["message_id"] = serde_json::Value::String(mid.clone());
            }
            if let Some(ref ct) = round_opts.chat_type {
                msg_meta.insert("chat_type".into(), serde_json::Value::String(ct.clone()));
            }
            if let Some(ref platform) = round_opts.platform {
                msg_meta.insert("platform".into(), serde_json::Value::String(platform.clone()));
            }
            if !round_opts.im_attachments.is_empty() {
                let json_str = serde_json::to_string(&round_opts.im_attachments).unwrap_or_default();
                let str_val = serde_json::Value::String(json_str);
                msg_meta.insert("im_attachments".into(), str_val.clone());
                meta["im_attachments"] = str_val;
            }
            if !round_opts.im_documents.is_empty() {
                let json_str = serde_json::to_string(&round_opts.im_documents).unwrap_or_default();
                let str_val = serde_json::Value::String(json_str);
                msg_meta.insert("im_documents".into(), str_val.clone());
                meta["im_documents"] = str_val;
            }
            let message_metadata = if msg_meta.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(msg_meta))
            };
            let content = prepend_group_sender_username(
                next_content.clone(),
                round_opts.chat_type.as_deref(),
                requested_user_name.as_deref(),
            );

            let should_log_media_input = content.is_multimodal()
                || !round_opts.im_attachments.is_empty()
                || !round_opts.im_documents.is_empty();
            if should_log_media_input && !self.model_profile.supports_images {
                yield CatEvent::Error(AgentError::other(format!(
                    "current model profile `{}` does not support image/document inputs; switch REMI_MODEL_PROFILE to a multimodal model",
                    self.model_profile.id
                )));
                return;
            }
            if should_log_media_input {
                tracing::info!(
                    thread_id = %thread_id_owned,
                    sender_user_id = opts.sender_user_id.as_deref().unwrap_or(""),
                    message_id = opts.message_id.as_deref().unwrap_or(""),
                    chat_type = opts.chat_type.as_deref().unwrap_or(""),
                    content_summary = %summarize_content_for_log(&content),
                    attachment_count = round_opts.im_attachments.len(),
                    document_count = round_opts.im_documents.len(),
                    "stream_with_options: media input"
                );
            }

            tracing::debug!(
                thread_id = %thread_id_owned,
                skip_count,
                has_message_metadata = message_metadata.is_some(),
                has_single_chat_sender_prompt = is_direct_chat(opts.chat_type.as_deref()) && (requested_user_name.is_some() || opts.sender_user_id.as_deref().is_some_and(|value| !value.trim().is_empty())),
                requested_user_name = requested_user_name.as_deref().unwrap_or(""),
                injected_user_name = injected_user_name.as_deref().unwrap_or(""),
                ?message_metadata,
                "stream_with_options: building LoopInput"
            );
            tracing::info!(
                thread_id = %thread_id_owned,
                sender_user_id = round_opts.sender_user_id.as_deref().unwrap_or(""),
                message_id = round_opts.message_id.as_deref().unwrap_or(""),
                sender_username = requested_user_name.as_deref().unwrap_or(""),
                injected_user_name = injected_user_name.as_deref().unwrap_or(""),
                has_single_chat_sender_prompt = is_direct_chat(round_opts.chat_type.as_deref()) && (requested_user_name.is_some() || round_opts.sender_user_id.as_deref().is_some_and(|value| !value.trim().is_empty())),
                has_sender_username = requested_user_name.is_some(),
                has_message_metadata = message_metadata.is_some(),
                "stream_with_options: username propagation"
            );

            let initial_user_state = ctx.user_state.clone();

            let mut input = LoopInput::start_content(content)
                .history(history)
                .metadata(meta)
                .user_state(ctx.user_state);
            if let Some(user_name) = injected_user_name {
                input = input.user_name(user_name);
            }
            if let Some(mm) = message_metadata {
                input = input.message_metadata(mm);
            }

            yield CatEvent::StateUpdate(initial_user_state);

            // 4. Drive inner agent, intercept History event to persist.
            let mut raw_history: Option<Vec<Message>> = None;
            let mut raw_user_state: Option<serde_json::Value> = None;
            let cancel = round_opts.cancel.clone();
            let inner_stream = self.inner.stream_with_input(input);
            let mut inner_stream = std::pin::pin!(inner_stream);

            loop {
                // When a cancel notify is present, race the inner stream against
                // the cancel signal so in-progress content is persisted even when
                // the task is preempted (e.g. by a newer incoming message).
                enum SelectOut {
                    Event(Option<CatEvent>),
                    Cancelled,
                }
                let outcome = if let Some(ref notify) = cancel {
                    tokio::select! {
                        ev = inner_stream.next() => SelectOut::Event(ev),
                        _ = notify.notified() => SelectOut::Cancelled,
                    }
                } else {
                    SelectOut::Event(inner_stream.next().await)
                };

                match outcome {
                    SelectOut::Cancelled => {
                        tracing::info!(
                            thread_id = %thread_id_owned,
                            "stream_with_options: cooperative cancel — persisting partial content"
                        );
                        persist_turn(
                            &self.memory, &thread_id_owned,
                            raw_history.take(), raw_user_state.take(), skip_count,
                        ).await;
                        yield CatEvent::Done;
                        return;
                    }
                    SelectOut::Event(None) => break,
                    SelectOut::Event(Some(ev)) => match ev {
                        CatEvent::History(msgs, us) => {
                            raw_history = Some(msgs);
                            raw_user_state = Some(us);
                        }
                        // Persist user_state immediately after each tool round.
                        CatEvent::StateUpdate(us) => {
                            yield persist_intermediate_user_state(
                                &self.memory,
                                &thread_id_owned,
                                us,
                            )
                            .await;
                        }
                        // Save memory BEFORE yielding Done/Error — the caller drops
                        // the stream immediately on these events, so any code after
                        // this loop would never execute.
                        CatEvent::Done => {
                            let supervisor_history = raw_history.clone();
                            persist_turn(
                                &self.memory, &thread_id_owned,
                                raw_history.take(), raw_user_state.take(), skip_count,
                            ).await;
                            if !round_opts.supervisor_run {
                                match self
                                    .evaluate_goal_after_round(
                                        &thread_id_owned,
                                        supervisor_history.as_deref().unwrap_or(&[]),
                                        supervisor_round,
                                    )
                                    .await
                                {
                                    GoalRoundOutcome::NoGoal => {}
                                    GoalRoundOutcome::Report(report) => {
                                        yield CatEvent::Supervisor(report);
                                    }
                                    GoalRoundOutcome::Continue { report, message } => {
                                        yield CatEvent::Supervisor(report);
                                        supervisor_round = supervisor_round.saturating_add(1);
                                        next_content = Content::text(message);
                                        continuation_from_supervisor = true;
                                        continue 'goal_loop;
                                    }
                                }
                            }
                            yield CatEvent::Done;
                            return;
                        }
                        CatEvent::Error(e) => {
                            // Best-effort save on error (partial history is better than nothing).
                            persist_turn(
                                &self.memory, &thread_id_owned,
                                raw_history.take(), raw_user_state.take(), skip_count,
                            ).await;
                            yield CatEvent::Error(e);
                            return;
                        }
                        other => yield other,
                    },
                }
            }

            // Fallback: stream ended without Done (shouldn't normally happen).
            persist_turn(
                &self.memory, &thread_id_owned,
                raw_history.take(), raw_user_state.take(), skip_count,
            ).await;
            break 'goal_loop;
            }
        }
    }
}

enum GoalRoundOutcome {
    NoGoal,
    Report(SupervisorReport),
    Continue {
        report: SupervisorReport,
        message: String,
    },
}

fn goal_round_allows_continue(max_rounds: &GoalMaxRounds, completed_continuations: u32) -> bool {
    match max_rounds {
        GoalMaxRounds::Limited(max) => completed_continuations < *max,
        GoalMaxRounds::Unlimited => true,
    }
}

// -- Helpers ------------------------------------------------------------------

/// Save new turn messages and user_state to the memory store.
///
/// Strips the first `skip_count` messages (the injected history prefix) from
/// `history` before persisting, so only the new user + assistant messages are
/// appended to short-term storage.
async fn persist_turn(
    memory: &MemoryStore,
    thread_id: &str,
    history: Option<Vec<Message>>,
    user_state: Option<serde_json::Value>,
    skip_count: usize,
) {
    if let Some(all_msgs) = history {
        let new_msgs: Vec<Message> = all_msgs.into_iter().skip(skip_count).collect();
        tracing::debug!(
            thread_id,
            skip_count,
            total_msgs = new_msgs.len(),
            msgs_with_metadata = new_msgs.iter().filter(|m| m.metadata.is_some()).count(),
            "persist_turn: saving messages"
        );
        for (i, m) in new_msgs.iter().enumerate() {
            tracing::debug!(
                i, role = ?m.role, has_metadata = m.metadata.is_some(), metadata = ?m.metadata,
                "persist_turn: message[{}]", i
            );
        }
        if !new_msgs.is_empty() {
            if let Err(e) = memory.save_turn(thread_id, new_msgs).await {
                tracing::warn!("memory save_turn failed: {e:#}");
            }
        }
    }
    if let Some(us) = user_state {
        if let Err(e) = memory.save_user_state(thread_id, &us).await {
            tracing::warn!("memory save_user_state failed: {e:#}");
        }
    }
}

async fn persist_intermediate_user_state(
    memory: &MemoryStore,
    thread_id: &str,
    user_state: serde_json::Value,
) -> CatEvent {
    if let Err(e) = memory.save_user_state(thread_id, &user_state).await {
        tracing::warn!("memory save_user_state (intermediate) failed: {e:#}");
    }
    CatEvent::StateUpdate(user_state)
}

fn truncate_user_name(name: Option<&str>, max_chars: usize) -> Option<String> {
    let trimmed = name?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(max_chars).collect())
}

fn insert_single_chat_sender_system_prompt(
    history: &mut Vec<Message>,
    insertion_index: usize,
    prompt: Option<String>,
) {
    let Some(prompt) = prompt else {
        return;
    };
    history.insert(insertion_index.min(history.len()), Message::system(prompt));
}

fn append_thread_todo_system_prompt(history: &mut Vec<Message>, user_state: &serde_json::Value) {
    if let Some(prompt) = todo::latest_unfinished_batch_system_prompt(user_state) {
        history.push(Message::system(prompt));
    }
}

fn single_chat_sender_system_prompt(
    chat_type: Option<&str>,
    sender_username: Option<&str>,
    sender_user_id: Option<&str>,
) -> Option<String> {
    if !is_direct_chat(chat_type) {
        return None;
    }

    let username = sender_username
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let sender_user_id = sender_user_id
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match (username, sender_user_id) {
        (Some(username), Some(sender_user_id)) => Some(format!(
            "当前是单聊场景。当前正在与你对话的用户是 {username}（内部ID: {sender_user_id}）。"
        )),
        (Some(username), None) => Some(format!(
            "当前是单聊场景。当前正在与你对话的用户是 {username}。"
        )),
        (None, Some(sender_user_id)) => Some(format!(
            "当前是单聊场景。当前正在与你对话的用户内部ID是 {sender_user_id}。"
        )),
        (None, None) => None,
    }
}

fn prepend_group_sender_username(
    content: Content,
    chat_type: Option<&str>,
    sender_username: Option<&str>,
) -> Content {
    let Some(prefix) = group_sender_prefix(chat_type, sender_username) else {
        return content;
    };

    match content {
        Content::Text(text) => Content::Text(format!("{prefix}{text}")),
        Content::Parts(mut parts) => {
            if let Some(ContentPart::Text { text }) = parts.first_mut() {
                let original = std::mem::take(text);
                *text = format!("{prefix}{original}");
            } else {
                parts.insert(0, ContentPart::text(prefix));
            }
            Content::Parts(parts)
        }
    }
}

fn group_sender_prefix(chat_type: Option<&str>, sender_username: Option<&str>) -> Option<String> {
    if !is_group_chat(chat_type) {
        return None;
    }

    let username = sender_username?.trim();
    if username.is_empty() {
        return None;
    }

    Some(format!("{username}:\n"))
}

fn is_group_chat(chat_type: Option<&str>) -> bool {
    chat_type
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("group"))
}

fn is_direct_chat(chat_type: Option<&str>) -> bool {
    chat_type
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("p2p"))
}

fn summarize_content_for_log(content: &Content) -> String {
    match content {
        Content::Text(text) => format!("text(len={})", text.chars().count()),
        Content::Parts(parts) => {
            let mut text_len = 0usize;
            let mut image_urls: Vec<String> = Vec::new();
            let mut image_base64: Vec<String> = Vec::new();
            let mut audio_parts = 0usize;
            let mut file_parts = 0usize;

            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        text_len += text.chars().count();
                    }
                    ContentPart::ImageUrl { image_url } => {
                        image_urls.push(format!(
                            "{}(len={})",
                            preview_url_header(&image_url.url),
                            image_url.url.len()
                        ));
                    }
                    ContentPart::ImageBase64 { media_type, data } => {
                        image_base64.push(format!("{}(data_len={})", media_type, data.len()));
                    }
                    ContentPart::Audio { .. } => {
                        audio_parts += 1;
                    }
                    ContentPart::File { .. } => {
                        file_parts += 1;
                    }
                }
            }

            format!(
                "parts(total={}, text_len={}, image_urls={:?}, image_base64={:?}, audio_parts={}, file_parts={})",
                parts.len(),
                text_len,
                image_urls,
                image_base64,
                audio_parts,
                file_parts,
            )
        }
    }
}

fn preview_url_header(url: &str) -> &str {
    match url.find(',') {
        Some(idx) => &url[..idx],
        None => url,
    }
}

fn trigger_command_tool_ctx(thread_id: &str, opts: &StreamOptions) -> ToolContext {
    ToolContext {
        config: AgentConfig::default(),
        thread_id: Some(
            serde_json::from_value(serde_json::json!(thread_id))
                .expect("thread_id should deserialize"),
        ),
        run_id: serde_json::from_value(serde_json::json!(format!("trigger-command:{thread_id}")))
            .expect("run_id should deserialize"),
        metadata: Some(trigger_command_metadata(thread_id, opts)),
        user_state: Arc::new(RwLock::new(serde_json::Value::Null)),
    }
}

fn trigger_command_metadata(thread_id: &str, opts: &StreamOptions) -> serde_json::Value {
    let mut meta = serde_json::json!({ "thread_id": thread_id });
    if let Some(ref sender_user_id) = opts.sender_user_id {
        meta["sender_user_id"] = serde_json::Value::String(sender_user_id.clone());
    }
    if let Some(ref sender_username) = opts.sender_username {
        meta["sender_username"] = serde_json::Value::String(sender_username.clone());
    }
    if let Some(ref message_id) = opts.message_id {
        meta["message_id"] = serde_json::Value::String(message_id.clone());
    }
    if let Some(ref chat_type) = opts.chat_type {
        meta["chat_type"] = serde_json::Value::String(chat_type.clone());
    }
    if let Some(ref platform) = opts.platform {
        meta["platform"] = serde_json::Value::String(platform.clone());
    }
    if opts.trigger_tools_enabled {
        meta["trigger_tools_enabled"] = serde_json::Value::String("true".to_string());
    }
    meta
}

// -- CatBotBuilder ------------------------------------------------------------

pub struct CatBotBuilder {
    api_key: String,
    model_profile: ModelProfileConfig,
    runtime_model_locked: bool,
    system: String,
    skills_dir: PathBuf,
    data_dir: PathBuf,
    /// If set, Agent.md is read from this path instead of `data_dir/Agent.md`.
    /// Allows placing Agent.md outside the agent's writable sandbox.
    agent_md_path: Option<PathBuf>,
    /// None → derive from model profile.
    short_term_tokens: Option<usize>,
    /// None → derive from model profile.
    overflow_bytes: Option<usize>,
    memory_days: u64,
    sandbox_config: SandboxConfig,
    im_bridge: Option<Arc<dyn ImFileBridge>>,
    extra_options: serde_json::Map<String, serde_json::Value>,
    tool_allowlist: Option<Vec<String>>,
    delegate_ids: Vec<String>,
    active_agent_id: String,
    model_bindings: AgentModelBindings,
    agents_dir: PathBuf,
    max_turns: Option<usize>,
    model_registry: Arc<ModelProfileRegistry>,
}

impl CatBotBuilder {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("REMI_API_KEY"))
            .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY or REMI_API_KEY must be set"))?;
        let memory_days = std::env::var("REMI_MEMORY_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7_u64);
        let bash_mode = match std::env::var("REMI_SHELL_MODE")
            .or_else(|_| std::env::var("REMI_BASH_MODE"))
            .as_deref()
        {
            Ok("local") => BashMode::Local,
            _ => BashMode::Docker,
        };
        let agent_md_path = std::env::var("AGENT_MD_PATH").ok().map(PathBuf::from);
        let data_dir =
            PathBuf::from(std::env::var("REMI_DATA_DIR").unwrap_or_else(|_| ".remi-cat".into()));
        let sandbox_config = SandboxConfig::from_env(data_dir.clone(), bash_mode);
        let skills_dir = data_dir.join("skills");
        let models_dir = data_dir.join("models");
        install_embedded_model_profiles(&models_dir)?;
        let model_registry = Arc::new(ModelProfileRegistry::load(&models_dir)?);
        let resolved_model = resolve_model_profile_from_env(&models_dir)?;
        let runtime_model_locked = std::env::var("REMI_MODEL_PROFILE")
            .ok()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let mut builder = Self {
            api_key,
            model_profile: resolved_model.profile,
            runtime_model_locked,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.clone(),
            agent_md_path,
            short_term_tokens: None,
            overflow_bytes: None,
            memory_days,
            sandbox_config,
            im_bridge: None,
            extra_options: resolved_model.extra_options,
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            agents_dir: data_dir.join("agents"),
            max_turns: None,
            model_registry,
        };
        let agent_id = std::env::var("REMI_AGENT_ID").unwrap_or_else(|_| DEFAULT_AGENT_ID.into());
        let agents_dir = std::env::var("REMI_AGENTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("agents"));
        builder.agents_dir = agents_dir.clone();
        if let Ok(registry) = AgentRegistry::load(agents_dir) {
            if let Some(profile) = registry.get(&agent_id) {
                builder = builder.agent_profile(profile.clone())?;
            }
        }
        Ok(builder)
    }

    pub fn system(mut self, s: impl Into<String>) -> Self {
        self.system = s.into();
        self
    }

    pub fn skills_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.skills_dir = dir.into();
        self
    }

    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = dir.into();
        self
    }

    /// Override the short-term memory token budget.
    /// By default this is derived from the model profile.
    pub fn short_term_tokens(mut self, n: usize) -> Self {
        self.short_term_tokens = Some(n);
        self
    }

    /// Override the tool-output overflow threshold (bytes).
    /// By default this is derived from the model profile.
    pub fn overflow_bytes(mut self, n: usize) -> Self {
        self.overflow_bytes = Some(n);
        self
    }

    pub fn im_bridge(mut self, bridge: Arc<dyn ImFileBridge>) -> Self {
        self.im_bridge = Some(bridge);
        self
    }

    pub fn agent_profile(mut self, profile: AgentProfile) -> anyhow::Result<Self> {
        if !self.runtime_model_locked {
            if let Some(model_profile_id) = profile.models.primary.as_deref() {
                let resolved = self
                    .model_registry
                    .get(model_profile_id)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "agent `{}` references unknown model profile `{}`",
                            profile.id,
                            model_profile_id
                        )
                    })?;
                self.model_profile = resolved;
            }
            if let Some(model) = profile.model.filter(|value| !value.trim().is_empty()) {
                self.model_profile.model = model;
            }
            if let Some(base_url) = profile.base_url.filter(|value| !value.trim().is_empty()) {
                self.model_profile.base_url = Some(base_url);
            }
        }
        self.model_bindings = profile.models.clone();
        self.active_agent_id = profile.id.clone();
        self.system = profile.system_prompt;
        let mut tools = profile.tools;
        for delegate in &profile.delegates {
            let name = delegate_tool_name(delegate);
            if !tools.iter().any(|tool| tool == &name) {
                tools.push(name);
            }
        }
        self.tool_allowlist = Some(tools);
        self.delegate_ids = profile.delegates;
        self.max_turns = profile.max_turns;
        Ok(self)
    }

    pub fn build(self) -> anyhow::Result<CatBot> {
        let profile = self.model_profile.clone();
        let short_term_tokens = self.short_term_tokens.unwrap_or(profile.short_term_tokens);
        let overflow_bytes = self.overflow_bytes.unwrap_or(profile.overflow_bytes);
        let resolved_base_url = profile.base_url.clone();

        tracing::debug!(
            model = %profile.model,
            profile = %profile.id,
            helper_model_profile = self.model_bindings.helper.as_deref().unwrap_or(""),
            vision_model_profile = self.model_bindings.vision.as_deref().unwrap_or(""),
            context_tokens = profile.context_tokens,
            max_output_tokens = profile.max_output_tokens,
            short_term_tokens,
            overflow_bytes,
            base_url = ?resolved_base_url,
            "model profile resolved"
        );

        if !self.extra_options.is_empty() {
            tracing::info!(
                model = %profile.model,
                extra_options = ?self.extra_options,
                "model extra options enabled"
            );
        }

        let mut oai = OpenAIClient::new(self.api_key.clone()).with_model(profile.model.clone());
        if let Some(url) = resolved_base_url.clone() {
            oai = oai.with_base_url(url);
        }
        let system_prompt = self.system.clone();

        let extra_options = self.extra_options.clone();
        let agent_config = AgentConfig::default().with_max_tokens(profile.max_output_tokens);
        let mut inner_builder = AgentBuilder::new()
            .model(oai)
            .config(agent_config)
            .system(system_prompt.clone())
            .max_turns(self.max_turns.unwrap_or(usize::MAX));
        if !extra_options.is_empty() {
            inner_builder = inner_builder.extra_options(extra_options.clone());
        }

        // ── LangSmith tracing (optional) ──────────────────────────────────
        if let Ok(api_key) = std::env::var("LANGSMITH_API_KEY") {
            if !api_key.is_empty() {
                let project =
                    std::env::var("LANGSMITH_PROJECT").unwrap_or_else(|_| "remi-cat".into());
                tracing::info!(project = %project, "LangSmith tracing enabled");
                inner_builder = inner_builder.tracer(
                    remi_agentloop::prelude::LangSmithTracer::new(api_key).with_project(project),
                );
            }
        }

        let inner_loop: InnerAgent = inner_builder.build_loop();

        // Compressor uses the same API credentials but no tools, max 1 turn.
        let compressor = LlmCompressor::new(
            self.api_key.clone(),
            resolved_base_url.clone(),
            profile.model.clone(),
            profile.max_output_tokens,
            extra_options,
        );

        let memory = Arc::new(MemoryStore {
            data_dir: self.data_dir,
            agent_md_path: self.agent_md_path,
            compressor,
            short_term_tokens,
            auto_compress: profile.auto_compress,
            memory_days: self.memory_days,
        });

        let skill_store = Arc::new(BuiltinSkillStore::new(
            FileSkillStore::new(self.skills_dir),
            [trigger::builtin_trigger_skill()],
        ));
        let data_dir = memory.data_dir.clone();
        let sandbox = self.sandbox_config.build()?;
        let agents_dir = self.agents_dir.clone();
        let active_agent_id = self.active_agent_id.clone();
        let todo_backend = Arc::new(todo::HybridTodoBackend::new(data_dir.clone()));
        let trigger_backend = Arc::new(trigger::TriggerBackend::new(data_dir.clone()));
        let acp_backend = Arc::new(acp::AcpBackend::new(
            data_dir.clone(),
            self.im_bridge.clone(),
        ));
        let mut acp_local_model =
            OpenAIClient::new(self.api_key.clone()).with_model(profile.model.clone());
        if let Some(url) = resolved_base_url.clone() {
            acp_local_model = acp_local_model.with_base_url(url);
        }
        let mut acp_local_builder = AgentBuilder::new()
            .model(acp_local_model)
            .config(AgentConfig::default().with_max_tokens(profile.max_output_tokens))
            .system(system_prompt)
            .max_turns(self.max_turns.unwrap_or(usize::MAX));
        if !self.extra_options.is_empty() {
            acp_local_builder = acp_local_builder.extra_options(self.extra_options.clone());
        }
        let acp_local_inner: InnerAgent = acp_local_builder.build_loop();
        let mut acp_local_tools = DefaultToolRegistry::new();
        skill::register_skill_tools(&mut acp_local_tools, Arc::clone(&skill_store));
        todo::register_todo_tools(&mut acp_local_tools, Arc::clone(&todo_backend));
        trigger::register_trigger_tools(&mut acp_local_tools, Arc::clone(&trigger_backend));
        profile::register_agent_tools(&mut acp_local_tools, agents_dir.clone());
        register_delegate_agent_tools(
            &mut acp_local_tools,
            &agents_dir,
            &self.delegate_ids,
            self.api_key.clone(),
            resolved_base_url.clone(),
            profile.model.clone(),
            self.extra_options.clone(),
        );
        acp_local_tools.register(MemoryGetDetailTool {
            store: Arc::clone(&memory),
        });
        acp_local_tools.register(MemoryUpsertNamedTool {
            store: Arc::clone(&memory),
            agent_id: active_agent_id.clone(),
        });
        acp_local_tools.register(MemoryRecallTool {
            store: Arc::clone(&memory),
            agent_id: active_agent_id.clone(),
        });
        acp_local_tools.register(SearchTool {
            skill_store: Arc::clone(&skill_store),
            memory_store: Arc::clone(&memory),
            agent_id: active_agent_id.clone(),
        });
        let acp_local_redactor: SharedRedactor =
            Arc::new(std::sync::RwLock::new(SecretRedactor::empty()));
        if self.sandbox_config.bash_enabled() {
            acp_local_tools.register(WorkspaceBashTool::new(
                Arc::clone(&sandbox),
                Arc::clone(&acp_local_redactor),
            ));
        }
        acp_local_tools.register(RootedFsReadTool {
            sandbox: Arc::clone(&sandbox),
            redactor: Arc::clone(&acp_local_redactor),
        });
        acp_local_tools.register(RootedFsWriteTool {
            sandbox: Arc::clone(&sandbox),
        });
        acp_local_tools.register(RootedFsApplyPatchTool {
            sandbox: Arc::clone(&sandbox),
        });
        acp_local_tools.register(RootedFsCreateTool {
            sandbox: Arc::clone(&sandbox),
        });
        acp_local_tools.register(RootedFsRemoveTool {
            sandbox: Arc::clone(&sandbox),
        });
        acp_local_tools.register(RootedFsLsTool {
            sandbox: Arc::clone(&sandbox),
            redactor: Arc::clone(&acp_local_redactor),
        });
        register_fetch_tool(
            &mut acp_local_tools,
            data_dir.clone(),
            self.im_bridge.clone(),
        );
        acp_local_tools.register(ExaSearchTool::new());
        acp_local_tools.register(NowTool);
        acp_local_tools.register(SleepTool);
        let run_locks: ThreadRunLocks = Arc::new(AsyncMutex::new(HashMap::new()));
        acp_backend.set_local_runner(Arc::new(LocalAcpAgentRunner {
            agent: CatAgent {
                inner: acp_local_inner,
                local_tools: acp_local_tools,
                data_dir: memory.data_dir.clone(),
                overflow_bytes,
                im_bridge: self.im_bridge.clone(),
                tool_allowlist: self.tool_allowlist.clone(),
            },
            memory: Arc::clone(&memory),
            run_locks: Arc::clone(&run_locks),
        }));
        let mut local_tools = DefaultToolRegistry::new();
        skill::register_skill_tools(&mut local_tools, Arc::clone(&skill_store));
        todo::register_todo_tools(&mut local_tools, Arc::clone(&todo_backend));
        trigger::register_trigger_tools(&mut local_tools, Arc::clone(&trigger_backend));
        acp::register_acp_tools(&mut local_tools, Arc::clone(&acp_backend));
        profile::register_agent_tools(&mut local_tools, agents_dir.clone());
        register_delegate_agent_tools(
            &mut local_tools,
            &agents_dir,
            &self.delegate_ids,
            self.api_key.clone(),
            resolved_base_url.clone(),
            profile.model.clone(),
            self.extra_options.clone(),
        );
        local_tools.register(MemoryGetDetailTool {
            store: Arc::clone(&memory),
        });
        local_tools.register(MemoryUpsertNamedTool {
            store: Arc::clone(&memory),
            agent_id: active_agent_id.clone(),
        });
        local_tools.register(MemoryRecallTool {
            store: Arc::clone(&memory),
            agent_id: active_agent_id.clone(),
        });
        local_tools.register(SearchTool {
            skill_store: Arc::clone(&skill_store),
            memory_store: Arc::clone(&memory),
            agent_id: active_agent_id,
        });

        // ── Workspace tools (bash + fs rooted at data_dir) ────────────────
        let redactor: SharedRedactor = Arc::new(std::sync::RwLock::new(SecretRedactor::empty()));
        if self.sandbox_config.bash_enabled() {
            local_tools.register(WorkspaceBashTool::new(
                Arc::clone(&sandbox),
                Arc::clone(&redactor),
            ));
        }
        local_tools.register(RootedFsReadTool {
            sandbox: Arc::clone(&sandbox),
            redactor: Arc::clone(&redactor),
        });
        local_tools.register(RootedFsWriteTool {
            sandbox: Arc::clone(&sandbox),
        });
        local_tools.register(RootedFsApplyPatchTool {
            sandbox: Arc::clone(&sandbox),
        });
        local_tools.register(RootedFsCreateTool {
            sandbox: Arc::clone(&sandbox),
        });
        local_tools.register(RootedFsRemoveTool {
            sandbox: Arc::clone(&sandbox),
        });
        local_tools.register(RootedFsLsTool {
            sandbox: Arc::clone(&sandbox),
            redactor: Arc::clone(&redactor),
        });

        register_fetch_tool(&mut local_tools, data_dir.clone(), self.im_bridge.clone());
        local_tools.register(ExaSearchTool::new());

        // ── Current time ──────────────────────────────────────────────────
        local_tools.register(NowTool);
        local_tools.register(SleepTool);

        Ok(CatBot {
            inner: CatAgent {
                inner: inner_loop,
                local_tools,
                data_dir: memory.data_dir.clone(),
                overflow_bytes,
                im_bridge: self.im_bridge,
                tool_allowlist: self.tool_allowlist,
            },
            memory,
            todo_backend,
            trigger_backend,
            acp_backend,
            run_locks,
            model_profile: profile,
            redactor,
        })
    }
}

fn delegate_tool_name(agent_id: &str) -> String {
    format!("agent__{}", agent_id.trim().replace('-', "_"))
}

fn register_delegate_agent_tools(
    registry: &mut DefaultToolRegistry,
    agents_dir: &std::path::Path,
    delegate_ids: &[String],
    api_key: String,
    base_url: Option<String>,
    default_model: String,
    extra_options: serde_json::Map<String, serde_json::Value>,
) {
    let Ok(agent_registry) = AgentRegistry::load(agents_dir) else {
        return;
    };
    for delegate_id in delegate_ids {
        let Some(profile) = agent_registry.get(delegate_id).cloned() else {
            tracing::warn!(delegate_id, "delegate agent profile not found");
            continue;
        };
        let tool_name = delegate_tool_name(&profile.id);
        let tool_description = format!(
            "Delegate a focused task to the `{}` sub-agent. {}",
            profile.name, profile.description
        );
        let agent_name = profile.id.clone();
        let system_prompt = profile.system_prompt.clone();
        let model_name = profile
            .model
            .clone()
            .unwrap_or_else(|| default_model.clone());
        let tool_base_url = profile.base_url.clone().or_else(|| base_url.clone());
        let max_turns = profile.max_turns.unwrap_or(12);
        let tool_api_key = api_key.clone();
        let tool_extra_options = extra_options.clone();
        registry.register(SubAgentToolAdapter::new(
            tool_name,
            tool_description,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Complete, self-contained task for the sub-agent. Include all necessary context."
                    }
                },
                "required": ["task"]
            }),
            agent_name,
            |arguments| {
                arguments
                    .get("task")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            },
            move |arguments, _ctx| {
                let task = arguments
                    .get("task")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let api_key = tool_api_key.clone();
                let model_name = model_name.clone();
                let base_url = tool_base_url.clone();
                let system_prompt = system_prompt.clone();
                let extra_options = tool_extra_options.clone();
                Box::pin(async move {
                    if task.trim().is_empty() {
                        return Err(AgentError::tool("sub-agent", "missing task"));
                    }
                    let mut model = OpenAIClient::new(api_key).with_model(model_name);
                    if let Some(url) = base_url {
                        model = model.with_base_url(url);
                    }
                    let mut builder = AgentBuilder::new()
                        .model(model)
                        .system(system_prompt)
                        .max_turns(max_turns);
                    if !extra_options.is_empty() {
                        builder = builder.extra_options(extra_options);
                    }
                    let agent = builder.build_loop();
                    Ok(Box::pin(stream! {
                        match agent.chat(LoopInput::start(&task)).await {
                            Ok(inner_stream) => {
                                let mut inner_stream = std::pin::pin!(inner_stream);
                                while let Some(event) = inner_stream.next().await {
                                    yield event;
                                }
                            }
                            Err(error) => {
                                yield AgentEvent::Error(error);
                            }
                        }
                    }) as SubAgentEventStream)
                })
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_thread_todo_system_prompt, default_system_prompt,
        insert_single_chat_sender_system_prompt, local_acp_thread_id,
        memory::{build_injected_history, MemoryContext, MemoryIndex},
        model_profile::ModelProfileConfig,
        prepend_group_sender_username, single_chat_sender_system_prompt, thread_run_lock,
        AgentModelBindings, CatBotBuilder, Content, ContentPart, GoalMaxRounds, Message,
        ModelProfileRegistry, SandboxConfig, ThreadRunLocks, DEFAULT_AGENT_ID,
    };
    use crate::todo::tools::TodoItem;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;
    use uuid::Uuid;

    fn test_model_profile() -> ModelProfileConfig {
        ModelProfileConfig {
            id: "default".to_string(),
            name: "Default".to_string(),
            description: None,
            provider: None,
            model: "gpt-4o".to_string(),
            base_url: Some("https://api.openai.com/v1".to_string()),
            thinking: None,
            max_output_tokens: 4096,
            context_tokens: 128000,
            supports_images: true,
            short_term_tokens: 8192,
            overflow_bytes: 16384,
            auto_compress: true,
            extra_options: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn thread_run_lock_is_scoped_by_thread_id() {
        let locks: ThreadRunLocks = Arc::new(AsyncMutex::new(HashMap::new()));

        let first = thread_run_lock(&locks, "session-a").await;
        let second = thread_run_lock(&locks, "session-a").await;
        let other = thread_run_lock(&locks, "session-b").await;

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[test]
    fn goal_round_limit_allows_default_twenty_continuations() {
        assert!(super::goal_round_allows_continue(
            &GoalMaxRounds::Limited(20),
            0
        ));
        assert!(super::goal_round_allows_continue(
            &GoalMaxRounds::Limited(20),
            19
        ));
        assert!(!super::goal_round_allows_continue(
            &GoalMaxRounds::Limited(20),
            20
        ));
        assert!(super::goal_round_allows_continue(
            &GoalMaxRounds::Unlimited,
            u32::MAX
        ));
    }

    #[test]
    fn agent_profile_delegates_are_exposed_as_tools() {
        let profile = crate::profile::AgentProfile::from_markdown(
            r#"---
id: default
name: Remi
description: General assistant
tools:
  - search
delegates:
  - coder
---
You are Remi.
"#,
        )
        .unwrap();
        let builder = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile: test_model_profile(),
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir: PathBuf::from("skills"),
            data_dir: PathBuf::from("data"),
            agent_md_path: None,
            short_term_tokens: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: PathBuf::from("data"),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            agents_dir: PathBuf::from("agents"),
            max_turns: None,
            model_registry: Arc::new(ModelProfileRegistry::load(PathBuf::from("models")).unwrap()),
        }
        .agent_profile(profile)
        .unwrap();
        let tools = builder.tool_allowlist.unwrap();
        assert!(tools.iter().any(|tool| tool == "search"));
        assert!(tools.iter().any(|tool| tool == "agent__coder"));
        assert_eq!(builder.delegate_ids, vec!["coder"]);
    }

    #[test]
    fn explicit_runtime_model_profile_is_not_overridden_by_agent_primary_model() {
        let profile = crate::profile::AgentProfile::from_markdown(
            r#"---
id: default
name: Remi
description: General assistant
models:
  primary: default
---
You are Remi.
"#,
        )
        .unwrap();
        let builder = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile: ModelProfileConfig {
                id: "deepseek-v4-flash".to_string(),
                name: "DeepSeek V4 Flash".to_string(),
                description: None,
                provider: None,
                model: "deepseek-v4-flash".to_string(),
                base_url: Some("https://api.deepseek.com".to_string()),
                thinking: None,
                max_output_tokens: 393216,
                context_tokens: 1_000_000,
                supports_images: false,
                short_term_tokens: 16000,
                overflow_bytes: 24000,
                auto_compress: true,
                extra_options: serde_json::Map::new(),
            },
            runtime_model_locked: true,
            system: default_system_prompt(),
            skills_dir: PathBuf::from("skills"),
            data_dir: PathBuf::from("data"),
            agent_md_path: None,
            short_term_tokens: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: PathBuf::from("data"),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            agents_dir: PathBuf::from("agents"),
            max_turns: None,
            model_registry: Arc::new(ModelProfileRegistry::load(PathBuf::from("models")).unwrap()),
        }
        .agent_profile(profile)
        .unwrap();
        assert_eq!(builder.model_profile.id, "deepseek-v4-flash");
        assert_eq!(builder.model_profile.model, "deepseek-v4-flash");
        assert_eq!(
            builder.model_profile.base_url.as_deref(),
            Some("https://api.deepseek.com")
        );
    }

    #[test]
    fn build_injects_local_acp_runner() {
        let data_dir = std::env::temp_dir().join(format!("remi-cat-acp-runner-{}", Uuid::new_v4()));
        let skills_dir = data_dir.join("skills");
        let agents_dir = data_dir.join("agents");
        let models_dir = data_dir.join("models");
        let builder = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile: test_model_profile(),
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.clone(),
            agent_md_path: None,
            short_term_tokens: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: data_dir.clone(),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
        };

        let bot = builder.build().unwrap();
        assert!(bot.acp_backend.has_local_runner());

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn local_acp_thread_id_is_session_scoped() {
        assert_eq!(local_acp_thread_id("session-1"), "acp:session-1");
    }

    #[test]
    fn group_text_content_is_prefixed_with_sender_username() {
        let content =
            prepend_group_sender_username(Content::text("hello"), Some("group"), Some("vv"));

        match content {
            Content::Text(text) => assert_eq!(text, "vv:\nhello"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn p2p_content_is_not_prefixed() {
        let content =
            prepend_group_sender_username(Content::text("hello"), Some("p2p"), Some("vv"));

        match content {
            Content::Text(text) => assert_eq!(text, "hello"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn group_multimodal_content_gets_prefix_text_part() {
        let content = prepend_group_sender_username(
            Content::parts(vec![ContentPart::image_url("data:image/png;base64,abc")]),
            Some("group"),
            Some("vv"),
        );

        match content {
            Content::Parts(parts) => {
                assert!(
                    matches!(parts.first(), Some(ContentPart::Text { text }) if text == "vv:\n")
                );
                assert!(matches!(parts.get(1), Some(ContentPart::ImageUrl { .. })));
            }
            other => panic!("expected parts content, got {other:?}"),
        }
    }

    #[test]
    fn single_chat_sender_system_prompt_is_inserted_after_soul() {
        let mut history = vec![
            Message::system("agent"),
            Message::system("soul"),
            Message::system("long-term"),
        ];

        insert_single_chat_sender_system_prompt(
            &mut history,
            2,
            single_chat_sender_system_prompt(Some("p2p"), Some("Alice"), Some("uuid-1")),
        );

        let contents: Vec<String> = history
            .iter()
            .map(|message| message.content.text_content())
            .collect();
        assert_eq!(contents[0], "agent");
        assert_eq!(contents[1], "soul");
        assert_eq!(
            contents[2],
            "当前是单聊场景。当前正在与你对话的用户是 Alice（内部ID: uuid-1）。"
        );
        assert_eq!(contents[3], "long-term");
    }

    #[test]
    fn group_chat_does_not_get_single_chat_sender_system_prompt() {
        assert!(
            single_chat_sender_system_prompt(Some("group"), Some("Alice"), Some("uuid-1"))
                .is_none()
        );
    }

    #[test]
    fn single_chat_sender_system_prompt_uses_uuid_when_username_missing() {
        let prompt = single_chat_sender_system_prompt(Some("p2p"), None, Some("uuid-1"));
        assert_eq!(
            prompt.as_deref(),
            Some("当前是单聊场景。当前正在与你对话的用户内部ID是 uuid-1。")
        );
    }

    #[test]
    fn thread_todo_batch_prompt_is_appended_after_history_context() {
        let user_state = json!({
            "__todos": [TodoItem {
                id: 1,
                content: "Draft changelog".to_string(),
                description: None,
                done: false,
                batch_id: Some(1),
                batch_title: Some("Release launch".to_string()),
                batch_index: Some(0),
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
            }]
        });
        let ctx = MemoryContext {
            agent_md: Some("agent".to_string()),
            soul_md: Some("soul".to_string()),
            long_term: MemoryIndex::default(),
            mid_term: MemoryIndex::default(),
            short_term: vec![
                Message::user("previous user"),
                Message::assistant("previous assistant"),
            ],
            user_state,
        };

        let mut history = build_injected_history(&ctx);
        insert_single_chat_sender_system_prompt(
            &mut history,
            2,
            single_chat_sender_system_prompt(Some("p2p"), Some("Alice"), Some("uuid-1")),
        );
        append_thread_todo_system_prompt(&mut history, &ctx.user_state);

        let contents: Vec<String> = history
            .iter()
            .map(|message| message.content.text_content())
            .collect();
        assert_eq!(contents[0], "agent");
        assert_eq!(contents[1], "soul");
        assert_eq!(
            contents[2],
            "当前是单聊场景。当前正在与你对话的用户是 Alice（内部ID: uuid-1）。"
        );
        assert_eq!(contents[3], "previous user");
        assert_eq!(contents[4], "previous assistant");
        assert_eq!(
            contents[5],
            "[CURRENT TODO BATCH]\nThis thread still has unfinished work under \"Release launch\".\nKeep progress synchronized with todo__complete/update/remove.\nWhen this thread has an active plan, try to complete multiple todo items in one pass whenever feasible. Only stop early if the user explicitly cancels, changes direction, or you need user input/help to proceed. Finish each individual todo item's work before marking it complete.\n- #1 Draft changelog"
        );
    }

    #[tokio::test]
    async fn intermediate_state_updates_are_persisted_and_forwarded() {
        let data_dir =
            std::env::temp_dir().join(format!("remi-cat-state-update-{}", Uuid::new_v4()));
        let memory = super::MemoryStore {
            data_dir: data_dir.clone(),
            agent_md_path: None,
            compressor: super::memory::LlmCompressor::new(
                "test-key".to_string(),
                None,
                "gpt-4o-mini".to_string(),
                4096,
                serde_json::Map::new(),
            ),
            short_term_tokens: 1024,
            auto_compress: true,
            memory_days: 7,
        };
        let user_state = json!({
            "__todos": [TodoItem {
                id: 6,
                content: "Summarize future trends".to_string(),
                description: None,
                done: true,
                batch_id: Some(1),
                batch_title: Some("Knowledge synthesis".to_string()),
                batch_index: Some(0),
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
            }]
        });

        let event =
            super::persist_intermediate_user_state(&memory, "thread-1", user_state.clone()).await;

        match event {
            super::CatEvent::StateUpdate(forwarded) => assert_eq!(forwarded, user_state),
            _ => panic!("expected StateUpdate event"),
        }

        let persisted = tokio::fs::read_to_string(data_dir.join("memory/thread-1/user_state.json"))
            .await
            .expect("intermediate user_state should be saved to disk");
        let persisted: serde_json::Value =
            serde_json::from_str(&persisted).expect("persisted user_state should deserialize");
        assert_eq!(persisted, user_state);

        let _ = tokio::fs::remove_dir_all(&data_dir).await;
    }
}

fn default_system_prompt() -> String {
    "You are a helpful AI assistant. \
     Use search for discovery and recall: by default scope=local searches local memory and skills; \
     set scope=memory for named/thread memory, scope=skills for skill catalog lookup, \
     scope=web for Exa web search, or scope=all for every available source. \
     If the user asks about their own prior facts, preferences, past actions, earlier conversations, \
     saved details, or anything phrased like \"my ...\", you MUST call search with scope=memory \
     and distinctive keywords from the question before giving the final answer. Do not say prior \
     information is unavailable until you have searched memory. \
     If the user asks you to remember, store, keep, or use a long note, transcript, document excerpt, \
     or historical conversation as future context, use memory__upsert_named to save a concise but \
     complete named memory before acknowledging it; do not rely only on short-term chat history. \
     When editing files, prefer apply_patch for focused changes to existing files. Use fs_write \
     only to create a new file or when you intentionally replace a whole file after reading it. \
     Use skill__get/skill__read_resource to inspect reusable procedures and related resources, todo tools \
     (todo__add/list/complete/update/remove) to track multi-step work per thread; \
     todo__add creates a titled batch of child todos and returns their IDs, \
     memory__upsert_named to save stable facts, preferences, and project conventions, \
     and memory__get_detail to read full compressed memory blocks. \
     Use them when appropriate."
        .to_string()
}
