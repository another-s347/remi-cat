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
pub mod im_tools;
pub mod memory;
pub mod model_profile;
pub mod skill;
pub mod todo;
pub mod tools;

pub use agent::CatAgent;
pub use events::{CatEvent, SkillEvent, TodoEvent};
pub use im_tools::{ImAttachment, ImDocument, ImFileBridge};
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

use crate::im_tools::register_fetch_tool;
use memory::{build_injected_history, LlmCompressor, MemoryGetDetailTool};
use tools::{
    BashMode, ExaSearchTool, NowTool, RootedFsCreateTool, RootedFsLsTool, RootedFsReadTool,
    RootedFsRemoveTool, RootedFsReplaceTool, RootedFsWriteTool, SecretRedactor, SleepTool,
    WorkspaceBashTool,
};

const REMI_KIMI_THINKING_ENV: &str = "REMI_KIMI_THINKING";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KimiThinkingMode {
    Auto,
    Enabled,
    Disabled,
}

impl KimiThinkingMode {
    fn from_env() -> anyhow::Result<Self> {
        match std::env::var(REMI_KIMI_THINKING_ENV) {
            Ok(raw) => Self::parse(&raw).ok_or_else(|| {
                anyhow::anyhow!("{REMI_KIMI_THINKING_ENV} must be one of: auto, enabled, disabled")
            }),
            Err(std::env::VarError::NotPresent) => Ok(Self::Disabled),
            Err(err) => Err(anyhow::anyhow!(
                "failed to read {REMI_KIMI_THINKING_ENV}: {err}"
            )),
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Some(Self::Auto),
            "enabled" | "enable" | "on" | "true" | "1" => Some(Self::Enabled),
            "disabled" | "disable" | "off" | "false" | "0" => Some(Self::Disabled),
            _ => None,
        }
    }

    fn request_type(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Enabled => Some("enabled"),
            Self::Disabled => Some("disabled"),
        }
    }
}

fn kimi_thinking_extra_options(
    model: &str,
    mode: KimiThinkingMode,
) -> serde_json::Map<String, serde_json::Value> {
    let mut options = serde_json::Map::new();
    let lower = model.trim().to_ascii_lowercase();

    let Some(thinking_type) = mode.request_type() else {
        return options;
    };

    if lower.contains("kimi-k2.5") {
        options.insert(
            "thinking".into(),
            serde_json::json!({ "type": thinking_type }),
        );
        return options;
    }

    if lower.contains("kimi-k2-thinking") {
        tracing::warn!(
            model = %model,
            env = REMI_KIMI_THINKING_ENV,
            "ignoring Kimi thinking override because kimi-k2-thinking always reasons"
        );
    } else if lower.contains("kimi") {
        tracing::warn!(
            model = %model,
            env = REMI_KIMI_THINKING_ENV,
            "ignoring Kimi thinking override because only kimi-k2.5 supports toggling thinking"
        );
    }

    options
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
    /// | `REMI_KIMI_THINKING`      | Moonshot `kimi-k2.5` thinking mode override: `auto`, `enabled`, or `disabled` (default when unset: `disabled`) |
    /// | `REMI_SHORT_TERM_TOKENS`  | Override short-term token budget (default: from model profile) |
    /// | `REMI_OVERFLOW_BYTES`     | Override tool-output overflow threshold in bytes (default: from model profile) |
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

            let requested_user_name = opts
                .sender_username
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let injected_user_name = requested_user_name
                .as_deref()
                .and_then(|value| truncate_user_name(Some(value), 10));
            let single_chat_sender_prompt = single_chat_sender_system_prompt(
                opts.chat_type.as_deref(),
                requested_user_name.as_deref(),
                opts.sender_user_id.as_deref(),
            );

            // 2. Build injected history prefix; record its length to strip later.
            let mut history = build_injected_history(&ctx);
            insert_single_chat_sender_system_prompt(
                &mut history,
                usize::from(ctx.agent_md.is_some()) + usize::from(ctx.soul_md.is_some()),
                single_chat_sender_prompt,
            );
            let skip_count = history.len();

            // 3. Build request-level metadata (thread_id for tools);
            //    build per-message metadata (sender identity + message id).
            let mut meta = serde_json::json!({ "thread_id": &thread_id_owned });
            if let Some(ref ct) = opts.chat_type {
                meta["chat_type"] = serde_json::Value::String(ct.clone());
            }
            if let Some(ref platform) = opts.platform {
                meta["platform"] = serde_json::Value::String(platform.clone());
            }

            let mut msg_meta = serde_json::Map::new();
            if let Some(ref sid) = opts.sender_user_id {
                msg_meta.insert("sender_user_id".into(), serde_json::Value::String(sid.clone()));
                meta["sender_user_id"] = serde_json::Value::String(sid.clone());
            }
            if let Some(ref username) = opts.sender_username {
                let username = username.trim();
                if !username.is_empty() {
                    let username = username.to_string();
                    msg_meta.insert("sender_username".into(), serde_json::Value::String(username.clone()));
                    meta["sender_username"] = serde_json::Value::String(username);
                }
            }
            if let Some(ref mid) = opts.message_id {
                msg_meta.insert("message_id".into(), serde_json::Value::String(mid.clone()));
                meta["message_id"] = serde_json::Value::String(mid.clone());
            }
            if let Some(ref ct) = opts.chat_type {
                msg_meta.insert("chat_type".into(), serde_json::Value::String(ct.clone()));
            }
            if let Some(ref platform) = opts.platform {
                msg_meta.insert("platform".into(), serde_json::Value::String(platform.clone()));
            }
            if !opts.im_attachments.is_empty() {
                let json_str = serde_json::to_string(&opts.im_attachments).unwrap_or_default();
                let str_val = serde_json::Value::String(json_str);
                msg_meta.insert("im_attachments".into(), str_val.clone());
                meta["im_attachments"] = str_val;
            }
            if !opts.im_documents.is_empty() {
                let json_str = serde_json::to_string(&opts.im_documents).unwrap_or_default();
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
                content,
                opts.chat_type.as_deref(),
                requested_user_name.as_deref(),
            );

            let should_log_media_input = content.is_multimodal()
                || !opts.im_attachments.is_empty()
                || !opts.im_documents.is_empty();
            if should_log_media_input {
                tracing::info!(
                    thread_id = %thread_id_owned,
                    sender_user_id = opts.sender_user_id.as_deref().unwrap_or(""),
                    message_id = opts.message_id.as_deref().unwrap_or(""),
                    chat_type = opts.chat_type.as_deref().unwrap_or(""),
                    content_summary = %summarize_content_for_log(&content),
                    attachment_count = opts.im_attachments.len(),
                    document_count = opts.im_documents.len(),
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
                sender_user_id = opts.sender_user_id.as_deref().unwrap_or(""),
                message_id = opts.message_id.as_deref().unwrap_or(""),
                sender_username = requested_user_name.as_deref().unwrap_or(""),
                injected_user_name = injected_user_name.as_deref().unwrap_or(""),
                has_single_chat_sender_prompt = is_direct_chat(opts.chat_type.as_deref()) && (requested_user_name.is_some() || opts.sender_user_id.as_deref().is_some_and(|value| !value.trim().is_empty())),
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
            let cancel = opts.cancel.clone();
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
                    },
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
    im_bridge: Option<Arc<dyn ImFileBridge>>,
    extra_options: serde_json::Map<String, serde_json::Value>,
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
            .ok()
            .filter(|s| !s.is_empty());
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
        let kimi_thinking_mode = KimiThinkingMode::from_env()?;
        let extra_options = kimi_thinking_extra_options(&model, kimi_thinking_mode);
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
            im_bridge: None,
            extra_options,
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

    pub fn im_bridge(mut self, bridge: Arc<dyn ImFileBridge>) -> Self {
        self.im_bridge = Some(bridge);
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

        if !self.extra_options.is_empty() {
            tracing::info!(
                model = %self.model,
                extra_options = ?self.extra_options,
                "model extra options enabled"
            );
        }

        let mut oai = OpenAIClient::new(self.api_key.clone()).with_model(self.model.clone());
        if let Some(url) = resolved_base_url.clone() {
            oai = oai.with_base_url(url);
        }

        let extra_options = self.extra_options.clone();
        let mut inner_builder = AgentBuilder::new()
            .model(oai)
            .system(self.system)
            .max_turns(usize::MAX);
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
        let compressor =
            LlmCompressor::new(self.api_key, resolved_base_url, self.model, extra_options);

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

        register_fetch_tool(&mut local_tools, data_dir.clone(), self.im_bridge.clone());

        // ── Web search (reads EXA_API_KEY from env at call time) ──────────
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
            },
            memory,
            redactor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        memory::{build_injected_history, MemoryContext, MemoryIndex},
        insert_single_chat_sender_system_prompt, kimi_thinking_extra_options,
        prepend_group_sender_username, single_chat_sender_system_prompt, Content, ContentPart,
        KimiThinkingMode, Message, REMI_KIMI_THINKING_ENV,
    };
    use crate::todo::tools::TodoItem;
    use serde_json::json;
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};
    use uuid::Uuid;

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                previous: std::env::var_os(key),
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                unsafe {
                    std::env::set_var(self.key, value);
                }
            } else {
                unsafe {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    fn kimi_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn kimi_k25_enabled_sets_thinking_extra_option() {
        let options = kimi_thinking_extra_options("kimi-k2.5", KimiThinkingMode::Enabled);
        assert_eq!(
            options.get("thinking"),
            Some(&serde_json::json!({ "type": "enabled" }))
        );
    }

    #[test]
    fn kimi_k25_disabled_sets_thinking_extra_option() {
        let options = kimi_thinking_extra_options("kimi-k2.5", KimiThinkingMode::Disabled);
        assert_eq!(
            options.get("thinking"),
            Some(&serde_json::json!({ "type": "disabled" }))
        );
    }

    #[test]
    fn non_toggleable_kimi_models_ignore_override() {
        let options = kimi_thinking_extra_options("kimi-k2-thinking", KimiThinkingMode::Disabled);
        assert!(options.is_empty());
    }

    #[test]
    fn kimi_thinking_defaults_to_disabled_when_env_missing() {
        let _lock = kimi_env_lock().lock().unwrap();
        let _guard = EnvVarGuard::capture(REMI_KIMI_THINKING_ENV);
        unsafe {
            std::env::remove_var(REMI_KIMI_THINKING_ENV);
        }

        assert_eq!(
            KimiThinkingMode::from_env().expect("default Kimi thinking mode should resolve"),
            KimiThinkingMode::Disabled
        );
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
    fn single_chat_sender_prompt_precedes_thread_todo_batch_prompt() {
        let user_state = json!({
            "__todos": [TodoItem {
                id: 1,
                content: "Draft changelog".to_string(),
                description: None,
                done: false,
                batch_id: Some(1),
                batch_title: Some("Release launch".to_string()),
                batch_index: Some(0),
            }]
        });
        let ctx = MemoryContext {
            agent_md: Some("agent".to_string()),
            soul_md: Some("soul".to_string()),
            long_term: MemoryIndex::default(),
            mid_term: MemoryIndex::default(),
            short_term: vec![],
            user_state,
        };

        let mut history = build_injected_history(&ctx);
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
        assert_eq!(
            contents[3],
            "[CURRENT TODO BATCH]\nThis thread still has unfinished work under \"Release launch\".\nKeep progress synchronized with todo__complete/update/remove.\nWhen this thread has an active plan, try to complete multiple todo items in one pass whenever feasible. Only stop early if the user explicitly cancels, changes direction, or you need user input/help to proceed. Finish each individual todo item's work before marking it complete.\n- #1 Draft changelog"
        );
    }

    #[tokio::test]
    async fn intermediate_state_updates_are_persisted_and_forwarded() {
        let data_dir = std::env::temp_dir().join(format!("remi-cat-state-update-{}", Uuid::new_v4()));
        let memory = super::MemoryStore {
            data_dir: data_dir.clone(),
            agent_md_path: None,
            compressor: super::memory::LlmCompressor::new(
                "test-key".to_string(),
                None,
                "gpt-4o-mini".to_string(),
                serde_json::Map::new(),
            ),
            short_term_tokens: 1024,
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
            }]
        });

        let event = super::persist_intermediate_user_state(&memory, "thread-1", user_state.clone()).await;

        match event {
            super::CatEvent::StateUpdate(forwarded) => assert_eq!(forwarded, user_state),
            _ => panic!("expected StateUpdate event"),
        }

        let persisted = tokio::fs::read_to_string(data_dir.join("memory/thread-1/user_state.json"))
            .await
            .expect("intermediate user_state should be saved to disk");
        let persisted: serde_json::Value = serde_json::from_str(&persisted)
            .expect("persisted user_state should deserialize");
        assert_eq!(persisted, user_state);

        let _ = tokio::fs::remove_dir_all(&data_dir).await;
    }
}

fn default_system_prompt() -> String {
    "You are a helpful AI assistant. \
     You have access to skill memory tools (skill__save/get/list/delete) \
     to store and recall reusable procedures, todo tools \
     (todo__add/list/complete/update/remove) to track multi-step work per thread; \
     todo__add creates a titled batch of child todos and returns their IDs, \
     and memory__get_detail to read full compressed memory blocks. \
     Use them when appropriate."
        .to_string()
}
