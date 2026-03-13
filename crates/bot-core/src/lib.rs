//! `bot-core` — DeepAgent re-implementation for remi-cat.
//!
//! ## Architecture
//!
//! ```text
//! CatBot
//!   └── CatAgent<AgentLoop<OpenAIClient>>
//!         ├── local_tools: skill__save / skill__get / skill__list / skill__delete
//!         ├── local_tools: todo__add / todo__list / todo__complete / todo__update / todo__remove
//!         └── local_tools: memory__get_detail
//!   └── MemoryStore  (shared via Arc)
//!         ├── .remi-cat/Agent.md + Soul.md  (injected as System messages every turn)
//!         ├── short_term.jsonl              (raw recent messages, per thread)
//!         ├── mid_term/<uuid>.md            (LLM-compressed, with raw archive)
//!         └── long_term/<uuid>.md           (LLM-compressed, with raw archive)
//! ```

pub mod agent;
pub mod events;
pub mod memory;
pub mod model_profile;
pub mod skill;
pub mod todo;
pub mod tools;

pub use agent::CatAgent;
pub use events::{CatEvent, SkillEvent, TodoEvent};
pub use memory::MemoryStore;
pub use model_profile::ModelProfile;
pub use remi_agentloop::prelude::{Content, ContentPart, Message};
pub use skill::store::{FileSkillStore, InMemorySkillStore};
pub use tools::SharedRedactor;

use std::path::PathBuf;
use std::sync::Arc;

use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_agentloop::agent_loop::AgentLoop;
use remi_agentloop::prelude::{AgentBuilder, LoopInput, OpenAIClient, ReqwestTransport};
use remi_agentloop::tool::registry::DefaultToolRegistry;

use memory::{build_injected_history, LlmCompressor, MemoryGetDetailTool};
use tools::{
    BashMode, ExaSearchTool, NowTool, RootedFsCreateTool, RootedFsLsTool, RootedFsReadTool,
    RootedFsRemoveTool, RootedFsReplaceTool, RootedFsWriteTool, SecretRedactor, WorkspaceBashTool,
};

// -- StreamOptions ----------------------------------------------------------

/// Per-turn options for [`CatBot::stream_with_options`].
#[derive(Debug, Default, Clone)]
pub struct StreamOptions {
    /// UUID of the sender (stored in metadata; injected as a
    /// system annotation in group chats so the LLM can distinguish speakers).
    pub sender_user_id: Option<String>,
    /// Feishu `message_id` of the incoming message (stored in metadata).
    pub message_id: Option<String>,
    /// Feishu `chat_type` — `"group"` or `"p2p"` (stored in metadata;
    /// triggers speaker annotation when `"group"`).
    pub chat_type: Option<String>,
}

// -- Type aliases -------------------------------------------------------------

type InnerAgent = AgentLoop<OpenAIClient<ReqwestTransport>>;

// -- CatBot -------------------------------------------------------------------

/// Main bot handle.  Build with [`CatBotBuilder`] or [`CatBot::from_env`].
pub struct CatBot {
    inner: CatAgent<InnerAgent>,
    memory: Arc<MemoryStore>,
    /// Shared secret redactor — updated via `update_secret_redactor`.
    redactor: SharedRedactor,
}

impl CatBot {
    /// Convenience constructor — reads credentials from environment variables.
    ///
    /// | Variable                  | Description                                           |
    /// |---------------------------|-------------------------------------------------------|
    /// | `OPENAI_API_KEY`          | API key (required)                                    |
    /// | `OPENAI_BASE_URL`         | Custom base URL (optional)                            |
    /// | `OPENAI_MODEL`            | Model name (default: `gpt-4o`)                        |
    /// | `REMI_SHORT_TERM_TOKENS`  | Override short-term token budget (default: from model profile) |
    /// | `REMI_OVERFLOW_BYTES`     | Override tool-output overflow threshold in bytes (default: from model profile) |
    /// | `REMI_MEMORY_DAYS`        | Days before mid-term → long-term (default: 7)         |
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
        self.memory.compact_now(thread_id).await
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
            .map(|d| (d.function.name, d.function.description))
            .collect()
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
            // 1. Load memory context (triggers mid->long-term promotion if needed).
            let ctx = match self.memory.load_context(&thread_id_owned).await {
                Ok(c) => c,
                Err(e) => { yield CatEvent::Error(e); return; }
            };

            // 2. Build injected history prefix; record its length to strip later.
            let history = build_injected_history(&ctx);
            let skip_count = history.len();

            // 3. Build request-level metadata (thread_id for tools);
            //    build per-message metadata (sender identity + message id).
            let mut meta = serde_json::json!({ "thread_id": &thread_id_owned });
            if let Some(ref ct) = opts.chat_type {
                meta["chat_type"] = serde_json::Value::String(ct.clone());
            }

            let mut msg_meta = serde_json::Map::new();
            if let Some(ref sid) = opts.sender_user_id {
                msg_meta.insert("sender_user_id".into(), serde_json::Value::String(sid.clone()));
                meta["sender_user_id"] = serde_json::Value::String(sid.clone());
            }
            if let Some(ref mid) = opts.message_id {
                msg_meta.insert("message_id".into(), serde_json::Value::String(mid.clone()));
            }
            if let Some(ref ct) = opts.chat_type {
                msg_meta.insert("chat_type".into(), serde_json::Value::String(ct.clone()));
            }
            let message_metadata = if msg_meta.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(msg_meta))
            };

            tracing::debug!(
                thread_id = %thread_id_owned,
                skip_count,
                has_message_metadata = message_metadata.is_some(),
                ?message_metadata,
                "stream_with_options: building LoopInput"
            );

            let mut input = LoopInput::start_content(content)
                .history(history)
                .metadata(meta)
                .user_state(ctx.user_state);
            if let Some(mm) = message_metadata {
                input = input.message_metadata(mm);
            }

            // 4. Drive inner agent, intercept History event to persist.
            let mut raw_history: Option<Vec<Message>> = None;
            let mut raw_user_state: Option<serde_json::Value> = None;
            let inner_stream = self.inner.stream_with_input(input);
            let mut inner_stream = std::pin::pin!(inner_stream);

            while let Some(ev) = inner_stream.next().await {
                match ev {
                    CatEvent::History(msgs, us) => {
                        raw_history = Some(msgs);
                        raw_user_state = Some(us);
                    }
                    // Persist user_state immediately after each tool round.
                    CatEvent::StateUpdate(us) => {
                        if let Err(e) = self.memory.save_user_state(&thread_id_owned, &us).await {
                            tracing::warn!("memory save_user_state (intermediate) failed: {e:#}");
                        }
                    }
                    // Save memory BEFORE yielding Done/Error — the caller drops
                    // the stream immediately on these events, so any code after
                    // this loop would never execute.
                    CatEvent::Done => {
                        persist_turn(
                            &self.memory, &thread_id_owned,
                            raw_history.take(), raw_user_state.take(), skip_count,
                        ).await;
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
                }
            }

            // Fallback: stream ended without Done (shouldn't normally happen).
            persist_turn(
                &self.memory, &thread_id_owned,
                raw_history.take(), raw_user_state.take(), skip_count,
            ).await;
        }
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

// -- CatBotBuilder ------------------------------------------------------------

pub struct CatBotBuilder {
    api_key: String,
    base_url: Option<String>,
    model: String,
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
    bash_mode: BashMode,
}

impl CatBotBuilder {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("REMI_API_KEY"))
            .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY or REMI_API_KEY must be set"))?;
        let model = std::env::var("OPENAI_MODEL")
            .or_else(|_| std::env::var("REMI_MODEL"))
            .unwrap_or_else(|_| "gpt-4o".into());
        let base_url = std::env::var("OPENAI_BASE_URL")
            .or_else(|_| std::env::var("REMI_BASE_URL"))
            .ok();
        let short_term_tokens = std::env::var("REMI_SHORT_TERM_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok());
        let overflow_bytes = std::env::var("REMI_OVERFLOW_BYTES")
            .ok()
            .and_then(|s| s.parse().ok());
        let memory_days = std::env::var("REMI_MEMORY_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7_u64);
        let bash_mode = match std::env::var("REMI_BASH_MODE").as_deref() {
            Ok("local") => BashMode::Local,
            _ => BashMode::Docker,
        };
        let agent_md_path = std::env::var("AGENT_MD_PATH").ok().map(PathBuf::from);
        Ok(Self {
            api_key,
            base_url,
            model,
            system: default_system_prompt(),
            skills_dir: PathBuf::from(".remi-cat/skills"),
            data_dir: PathBuf::from(".remi-cat"),
            agent_md_path,
            short_term_tokens,
            overflow_bytes,
            memory_days,
            bash_mode,
        })
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

    pub fn build(self) -> anyhow::Result<CatBot> {
        // Resolve model profile first so we can derive all configuration.
        let profile = ModelProfile::for_model(&self.model);
        let short_term_tokens = self
            .short_term_tokens
            .unwrap_or_else(|| profile.default_short_term_tokens());
        let overflow_bytes = self
            .overflow_bytes
            .unwrap_or_else(|| profile.default_overflow_bytes());
        // Explicit base_url wins; fall back to the profile's canonical URL.
        let resolved_base_url: Option<String> = self
            .base_url
            .clone()
            .or_else(|| profile.default_base_url().map(str::to_owned));

        tracing::debug!(
            model = %self.model,
            profile = %profile.name,
            context_tokens = profile.context_tokens,
            short_term_tokens,
            overflow_bytes,
            base_url = ?resolved_base_url,
            "model profile resolved"
        );

        let mut oai = OpenAIClient::new(self.api_key.clone()).with_model(self.model.clone());
        if let Some(url) = resolved_base_url.clone() {
            oai = oai.with_base_url(url);
        }

        let inner_loop: InnerAgent = AgentBuilder::new()
            .model(oai)
            .system(self.system)
            .max_turns(usize::MAX)
            .build_loop();

        // Compressor uses the same API credentials but no tools, max 1 turn.
        let compressor = LlmCompressor::new(self.api_key, resolved_base_url, self.model);

        let memory = Arc::new(MemoryStore {
            data_dir: self.data_dir,
            agent_md_path: self.agent_md_path,
            compressor,
            short_term_tokens,
            memory_days: self.memory_days,
        });

        let skill_store = Arc::new(FileSkillStore::new(self.skills_dir));
        let data_dir = memory.data_dir.clone();
        let mut local_tools = DefaultToolRegistry::new();
        skill::register_skill_tools(&mut local_tools, skill_store);
        todo::register_todo_tools(&mut local_tools);
        local_tools.register(MemoryGetDetailTool {
            store: Arc::clone(&memory),
        });

        // ── Workspace tools (bash + fs rooted at data_dir) ────────────────
        let redactor: SharedRedactor = Arc::new(std::sync::RwLock::new(SecretRedactor::empty()));
        local_tools.register(WorkspaceBashTool::new(
            data_dir.clone(),
            Arc::clone(&redactor),
            self.bash_mode,
        ));
        local_tools.register(RootedFsReadTool {
            root: data_dir.clone(),
            redactor: Arc::clone(&redactor),
        });
        local_tools.register(RootedFsWriteTool {
            root: data_dir.clone(),
        });
        local_tools.register(RootedFsReplaceTool {
            root: data_dir.clone(),
        });
        local_tools.register(RootedFsCreateTool {
            root: data_dir.clone(),
        });
        local_tools.register(RootedFsRemoveTool {
            root: data_dir.clone(),
        });
        local_tools.register(RootedFsLsTool {
            root: data_dir.clone(),
            redactor: Arc::clone(&redactor),
        });

        // ── Web search (reads EXA_API_KEY from env at call time) ──────────
        local_tools.register(ExaSearchTool::new());

        // ── Current time ──────────────────────────────────────────────────
        local_tools.register(NowTool);

        Ok(CatBot {
            inner: CatAgent {
                inner: inner_loop,
                local_tools,
                data_dir: memory.data_dir.clone(),
                overflow_bytes,
            },
            memory,
            redactor,
        })
    }
}

fn default_system_prompt() -> String {
    "You are a helpful AI assistant. \
     You have access to skill memory tools (skill__save/get/list/delete) \
     to store and recall reusable procedures, todo tools \
     (todo__add/list/complete/update/remove) to track multi-step work, \
     and memory__get_detail to read full compressed memory blocks. \
     Use them when appropriate."
        .to_string()
}
