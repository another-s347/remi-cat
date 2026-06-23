//! Chat runtime with actor-based state management.
//!
//! The runtime owns chat execution, survives UI lifecycle changes,
//! handles interrupts automatically, and supports manual resume.

use crate::TriggerSdk;
use crate::chat_agent::{SharedChatAgent, build_chat_agent, default_chat_agent};
use crate::chat_client::proto as chat_proto;
use crate::chat_client::{ChatStreamEvent, chat_stream_event};
use crate::chat_types::*;
use crate::external_tool_schema;
use crate::external_tools::{
    ExternalToolCallRequest, ExternalToolExecutor, manual_resume_outcomes,
};
use crate::local_wasm::ChatEventStream;
use crate::remi_uri::{RemiUri, mime_from_extension};
use base64::Engine as _;
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{RwLock, mpsc, oneshot};
use tokio_stream::StreamExt;

fn tool_outcome_shape(outcome: &ToolExecutionOutcome) -> &'static str {
    if outcome.result.is_some() {
        "result"
    } else if outcome.result_parts.is_some() {
        "parts"
    } else if outcome.error.is_some() {
        "error"
    } else {
        "empty"
    }
}

fn summarize_tool_outcome(outcome: &ToolExecutionOutcome) -> String {
    let detail = if let Some(result) = &outcome.result {
        format!("len={}", result.len())
    } else if let Some(parts) = &outcome.result_parts {
        format!("parts={}", parts.len())
    } else if let Some(error) = &outcome.error {
        format!("len={}", error.len())
    } else {
        "len=0".to_string()
    };

    format!(
        "{}:{}:{}:{}",
        outcome.tool_call_id,
        outcome.tool_name,
        tool_outcome_shape(outcome),
        detail
    )
}

fn summarize_protocol_history_message(message: &ProtocolHistoryMessage) -> String {
    let content_shape = match &message.content {
        JsonValue::String(text) => format!("text:{}", text.len()),
        JsonValue::Array(parts) => format!("parts:{}", parts.len()),
        JsonValue::Object(map) => format!("object:{}", map.len()),
        JsonValue::Null => "null".to_string(),
        JsonValue::Bool(_) => "bool".to_string(),
        JsonValue::Number(_) => "number".to_string(),
    };

    format!(
        "{}:{}:tool_calls={}:tool_call_id={}:{}",
        message.id,
        message.role,
        message.tool_calls.len(),
        message.tool_call_id.as_deref().unwrap_or("-"),
        content_shape,
    )
}

fn serialize_tool_outcomes_for_resume(
    session_id: &str,
    source: &str,
    outcomes: &[ToolExecutionOutcome],
) -> Vec<chat_proto::ChatToolCallOutcome> {
    let mut serialized = Vec::with_capacity(outcomes.len());
    let mut dropped = Vec::new();

    for outcome in outcomes {
        match tool_outcome_to_proto(outcome) {
            Some(proto) => serialized.push(proto),
            None => dropped.push(summarize_tool_outcome(outcome)),
        }
    }

    tracing::info!(
        session_id = %session_id,
        source,
        total_results = outcomes.len(),
        serialized_results = serialized.len(),
        dropped_results = dropped.len(),
        results = ?outcomes.iter().map(summarize_tool_outcome).collect::<Vec<_>>(),
        dropped = ?dropped,
        "Prepared tool outcomes for chat resume"
    );

    serialized
}

fn extract_fatal_error_from_event(event: &ChatStreamEvent) -> Option<String> {
    match event.event.as_ref()? {
        chat_stream_event::Event::Error(err) => Some(if err.message.trim().is_empty() {
            "Agent error".to_string()
        } else {
            err.message.clone()
        }),
        _ => None,
    }
}

#[derive(Clone)]
struct ChatExecutionBackend(SharedChatAgent);

impl Default for ChatExecutionBackend {
    fn default() -> Self {
        Self(default_chat_agent())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Session State
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Default)]
struct SessionState {
    messages: Vec<CachedMessage>,
    index: HashMap<String, usize>,
    version: u64,
    run_state: ChatRunState,
    last_error: Option<String>,
    pending_interrupt: Option<PendingInterrupt>,
    cancel_tx: Option<mpsc::Sender<()>>,
    current_run_id: Option<u64>,
    protocol_state: ChatProtocolSessionState,
    protocol_loaded: bool,
    /// Accumulated content by message ID (for streaming)
    assistant_content: HashMap<String, String>,
    /// Accumulated thinking by message ID
    assistant_thinking: HashMap<String, String>,

    /// Accumulated tool call args by tool_call_id (chunked, indexed)
    tool_call_args: HashMap<String, BTreeMap<i32, String>>,
    /// Best-known tool name by tool_call_id
    tool_call_name: HashMap<String, String>,
    /// Accumulated tool result content by tool_call_id
    tool_result_content: HashMap<String, String>,
    /// Structured sub-session drafts keyed by parent tool_call_id.
    sub_sessions: HashMap<String, ProtocolSubSessionDraft>,
}

impl SessionState {
    fn upsert(&mut self, mut msg: CachedMessage) {
        msg.refresh_ui_elements();
        let id = msg.id.clone();
        if let Some(&idx) = self.index.get(&id) {
            self.messages[idx] = msg;
        } else {
            let idx = self.messages.len();
            self.messages.push(msg);
            self.index.insert(id, idx);
        }
        self.version += 1;
    }

    fn get_messages(&self) -> Vec<CachedMessage> {
        self.messages.clone()
    }
}

#[derive(Default)]
struct ProtocolTurnState {
    pending_user: Option<ProtocolHistoryMessage>,
    assistant_content: String,
    assistant_reasoning: Option<String>,
    tool_calls: Vec<ProtocolToolCallDraft>,
    tool_messages: Vec<ProtocolHistoryMessage>,
    committed: bool,
}

#[derive(Clone)]
struct ProtocolToolCallDraft {
    id: String,
    tool_name: String,
    arguments_json: String,
}

#[derive(Clone)]
struct ProtocolSubSessionDraft {
    cached: CachedSubSession,
    tool_call_items: HashMap<String, usize>,
    tool_result_items: HashMap<String, usize>,
}

impl ProtocolSubSessionDraft {
    fn new(
        parent_tool_call_id: String,
        sub_session_id: String,
        sub_run_id: String,
        agent_name: String,
        title: Option<String>,
        depth: u32,
    ) -> Self {
        Self {
            cached: CachedSubSession {
                parent_tool_call_id,
                sub_session_id,
                sub_run_id,
                agent_name,
                title,
                depth,
                items: Vec::new(),
                final_output: None,
                status: Some("running".to_string()),
            },
            tool_call_items: HashMap::new(),
            tool_result_items: HashMap::new(),
        }
    }

    fn from_cached(cached: CachedSubSession) -> Self {
        Self {
            cached,
            tool_call_items: HashMap::new(),
            tool_result_items: HashMap::new(),
        }
    }

    fn snapshot(&self) -> CachedSubSession {
        self.cached.clone()
    }

    fn append_markdown(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }

        match self.cached.items.last_mut() {
            Some(CachedSubSessionItem::Markdown { text }) => text.push_str(delta),
            _ => self.cached.items.push(CachedSubSessionItem::Markdown {
                text: delta.to_string(),
            }),
        }
    }

    fn push_thinking(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.cached
            .items
            .push(CachedSubSessionItem::Thinking { text });
    }

    fn upsert_tool_call_start(&mut self, id: &str, tool_name: &str) {
        if let Some(index) = self.tool_call_items.get(id).copied() {
            if let Some(CachedSubSessionItem::ToolCall {
                tool_name: existing_name,
                ..
            }) = self.cached.items.get_mut(index)
            {
                *existing_name = tool_name.to_string();
            }
            return;
        }

        let index = self.cached.items.len();
        self.cached.items.push(CachedSubSessionItem::ToolCall {
            tool_name: tool_name.to_string(),
            arguments_json: None,
        });
        self.tool_call_items.insert(id.to_string(), index);
    }

    fn append_tool_call_delta(&mut self, id: &str, delta: &str) {
        if delta.is_empty() {
            return;
        }

        let index = if let Some(index) = self.tool_call_items.get(id).copied() {
            index
        } else {
            let index = self.cached.items.len();
            self.cached.items.push(CachedSubSessionItem::ToolCall {
                tool_name: "(unknown)".to_string(),
                arguments_json: Some(delta.to_string()),
            });
            self.tool_call_items.insert(id.to_string(), index);
            return;
        };

        if let Some(CachedSubSessionItem::ToolCall { arguments_json, .. }) =
            self.cached.items.get_mut(index)
        {
            let entry = arguments_json.get_or_insert_with(String::new);
            entry.push_str(delta);
        }
    }

    fn append_tool_result_delta(&mut self, id: &str, tool_name: &str, delta: &str) {
        if delta.is_empty() {
            return;
        }

        let index = self
            .tool_result_items
            .entry(id.to_string())
            .or_insert_with(|| {
                let index = self.cached.items.len();
                self.cached.items.push(CachedSubSessionItem::ToolResult {
                    tool_name: tool_name.to_string(),
                    result: String::new(),
                });
                index
            })
            .to_owned();

        if let Some(CachedSubSessionItem::ToolResult {
            tool_name: existing_name,
            result,
        }) = self.cached.items.get_mut(index)
        {
            if !tool_name.trim().is_empty() {
                *existing_name = tool_name.to_string();
            }
            result.push_str(delta);
        }
    }

    fn set_tool_result(&mut self, id: &str, tool_name: &str, result: String) {
        let index = self
            .tool_result_items
            .entry(id.to_string())
            .or_insert_with(|| {
                let index = self.cached.items.len();
                self.cached.items.push(CachedSubSessionItem::ToolResult {
                    tool_name: tool_name.to_string(),
                    result: String::new(),
                });
                index
            })
            .to_owned();

        if let Some(CachedSubSessionItem::ToolResult {
            tool_name: existing_name,
            result: existing_result,
        }) = self.cached.items.get_mut(index)
        {
            if !tool_name.trim().is_empty() {
                *existing_name = tool_name.to_string();
            }
            *existing_result = result;
        }
    }

    fn push_turn_start(&mut self, turn: u32) {
        self.cached
            .items
            .push(CachedSubSessionItem::TurnStart { turn });
    }

    fn finish(&mut self, final_output: Option<String>) {
        self.cached.final_output = final_output.filter(|value| !value.trim().is_empty());
        self.cached.status = Some("completed".to_string());
    }

    fn fail(&mut self, message: String) {
        self.cached.status = Some("error".to_string());
        self.cached
            .items
            .push(CachedSubSessionItem::Error { text: message });
    }
}

fn combined_tool_call_arguments(session: &SessionState, tool_call_id: &str) -> Option<String> {
    let args = session
        .tool_call_args
        .get(tool_call_id)
        .map(|chunks| chunks.values().cloned().collect::<String>())
        .unwrap_or_default();

    if args.trim().is_empty() {
        None
    } else {
        Some(args)
    }
}

fn build_tool_call_message(
    session: &SessionState,
    tool_call_id: &str,
    now_ms: i64,
) -> CachedMessage {
    let tool_name = session
        .tool_call_name
        .get(tool_call_id)
        .cloned()
        .unwrap_or_else(|| "(unknown)".to_string());
    let arguments_json = combined_tool_call_arguments(session, tool_call_id);

    let mut message = CachedMessage::assistant(format!("tool_call:{}", tool_call_id), now_ms);
    message.content = match arguments_json.as_ref() {
        Some(arguments) => format!("Tool call: `{}`\n\n```json\n{}\n```", tool_name, arguments),
        None => format!("Tool call: `{}`", tool_name),
    };
    message.tool_name = Some(tool_name);
    message.tool_call_id = Some(tool_call_id.to_string());
    message.sub_session = session
        .sub_sessions
        .get(tool_call_id)
        .map(ProtocolSubSessionDraft::snapshot);
    message
}

fn attach_sub_session_to_parent_tool_call(
    session: &mut SessionState,
    tool_call_id: &str,
    now_ms: i64,
) {
    if !session.sub_sessions.contains_key(tool_call_id) {
        return;
    }

    let message = build_tool_call_message(session, tool_call_id, now_ms);
    session.upsert(message);
}

impl ProtocolTurnState {
    fn for_user(message: ProtocolHistoryMessage) -> Self {
        Self {
            pending_user: Some(message),
            ..Self::default()
        }
    }

    fn record_delta(&mut self, delta: &str) {
        self.assistant_content.push_str(delta);
    }

    fn record_reasoning(&mut self, thinking: &str) {
        self.assistant_reasoning = Some(thinking.to_string());
    }

    fn record_tool_call_start(&mut self, id: &str, tool_name: &str) {
        if self.tool_calls.iter().all(|call| call.id != id) {
            self.tool_calls.push(ProtocolToolCallDraft {
                id: id.to_string(),
                tool_name: tool_name.to_string(),
                arguments_json: String::new(),
            });
        }
    }

    fn record_tool_call_delta(&mut self, id: &str, delta: &str) {
        if let Some(call) = self.tool_calls.iter_mut().find(|call| call.id == id) {
            call.arguments_json.push_str(delta);
            return;
        }

        self.tool_calls.push(ProtocolToolCallDraft {
            id: id.to_string(),
            tool_name: "unknown".to_string(),
            arguments_json: delta.to_string(),
        });
    }

    fn assistant_message(&self, assistant_id: &str) -> Option<ProtocolHistoryMessage> {
        let tool_calls = self
            .tool_calls
            .iter()
            .map(|call| ProtocolToolCall {
                id: call.id.clone(),
                tool_name: call.tool_name.clone(),
                arguments: parse_tool_arguments_json(&call.arguments_json),
            })
            .collect::<Vec<_>>();

        if self.assistant_content.trim().is_empty()
            && tool_calls.is_empty()
            && self.assistant_reasoning.is_none()
        {
            return None;
        }

        Some(ProtocolHistoryMessage {
            id: assistant_id.to_string(),
            role: "assistant".to_string(),
            content: JsonValue::String(self.assistant_content.clone()),
            tool_calls,
            tool_call_id: None,
            reasoning_content: self.assistant_reasoning.clone(),
        })
    }

    fn record_tool_result_message(&mut self, outcome: &ToolExecutionOutcome) {
        if self
            .tool_messages
            .iter()
            .any(|message| message.tool_call_id.as_deref() == Some(outcome.tool_call_id.as_str()))
        {
            return;
        }

        self.tool_messages
            .push(outcome_to_protocol_tool_message(outcome));
    }

    fn has_tool_phase(&self) -> bool {
        !self.tool_calls.is_empty() || !self.tool_messages.is_empty()
    }

    fn flush_phase_into_history(&mut self, session: &mut SessionState, assistant_id: &str) {
        if !self.committed {
            if let Some(user) = self.pending_user.take() {
                session.protocol_state.history.push(user);
            }
            if let Some(assistant) = self.assistant_message(assistant_id) {
                session.protocol_state.history.push(assistant);
            }
        }
        session
            .protocol_state
            .history
            .extend(self.tool_messages.drain(..));
        session.protocol_state.pending_tool_execution = None;
        self.assistant_content.clear();
        self.assistant_reasoning = None;
        self.tool_calls.clear();
        self.committed = false;
    }

    fn commit_completed(&mut self, session: &mut SessionState, assistant_id: &str) {
        if self.committed {
            return;
        }

        if let Some(user) = self.pending_user.take() {
            session.protocol_state.history.push(user);
        }
        if let Some(assistant) = self.assistant_message(assistant_id) {
            session.protocol_state.history.push(assistant);
        }
        session
            .protocol_state
            .history
            .extend(self.tool_messages.drain(..));
        session.protocol_state.pending_tool_execution = None;
        self.committed = true;
    }

    fn commit_need_tool_execution(
        &mut self,
        session: &mut SessionState,
        assistant_id: &str,
        pending: PendingToolExecutionState,
    ) {
        if self.committed {
            session.protocol_state.pending_tool_execution = Some(pending);
            return;
        }

        if let Some(user) = self.pending_user.take() {
            session.protocol_state.history.push(user);
        }
        if let Some(assistant) = self.assistant_message(assistant_id) {
            session.protocol_state.history.push(assistant);
        }
        session
            .protocol_state
            .history
            .extend(self.tool_messages.drain(..));
        session.protocol_state.pending_tool_execution = Some(pending);
        self.committed = true;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Actor Commands
// ═══════════════════════════════════════════════════════════════════════════════

enum Command {
    /// Initialize/reconfigure the runtime
    Init {
        access_token: String,
        config: ChatRuntimeConfig,
        reply: oneshot::Sender<Result<(), String>>,
    },

    /// Update access token
    UpdateToken {
        access_token: String,
        reply: oneshot::Sender<()>,
    },

    /// Send a message and start streaming
    SendMessage {
        session_id: String,
        message: String,
        system_prompt: Option<String>,
        references: Option<JsonValue>,
        attachments: Option<JsonValue>,
        active_agent: Option<String>,
        configured_external_tool_names: Option<Vec<String>>,
        user_msg_id: Option<String>,
        assistant_msg_id: Option<String>,
        reply: oneshot::Sender<Result<(), String>>,
    },

    /// Resume after WaitingForUser
    Resume {
        session_id: String,
        resume_value: JsonValue,
        reply: oneshot::Sender<Result<(), String>>,
    },

    /// Cancel one session or every in-flight session.
    Cancel {
        session_id: Option<String>,
        reply: oneshot::Sender<()>,
    },

    /// Get current status
    GetStatus {
        reply: oneshot::Sender<ChatRunStatus>,
    },

    /// Get status for a specific session
    GetSessionStatus {
        session_id: String,
        reply: oneshot::Sender<ChatRunStatus>,
    },

    /// Get messages for a session
    GetMessages {
        session_id: String,
        reply: oneshot::Sender<Vec<CachedMessage>>,
    },

    /// Get cache version for a session
    GetCacheVersion {
        session_id: String,
        reply: oneshot::Sender<u64>,
    },

    /// Initialize cache from existing messages
    InitCache {
        session_id: String,
        messages: Vec<CachedMessage>,
        reply: oneshot::Sender<()>,
    },

    /// Clear session cache
    ClearCache {
        session_id: String,
        reply: oneshot::Sender<()>,
    },

    /// Register unified external tool executor
    SetExternalToolExecutor {
        executor: ExternalToolExecutor,
        reply: oneshot::Sender<()>,
    },

    /// Subscribe to events
    Subscribe {
        tx: mpsc::Sender<ChatRuntimeEvent>,
        reply: oneshot::Sender<()>,
    },
}

// ═══════════════════════════════════════════════════════════════════════════════
// Actor State
// ═══════════════════════════════════════════════════════════════════════════════

struct ActorState {
    // Config
    access_token: Option<String>,
    config: ChatRuntimeConfig,
    backend: ChatExecutionBackend,

    // Sessions
    sessions: HashMap<String, SessionState>,

    // Last emitted status for compatibility with legacy polling APIs.
    latest_status: ChatRunStatus,
    next_run_id: u64,

    // External tool execution
    external_tool_executor: ExternalToolExecutor,

    // Event subscribers
    subscribers: Vec<mpsc::Sender<ChatRuntimeEvent>>,

    // SDK reference for DB persistence
    sdk: Option<Arc<TriggerSdk>>,
}

impl ActorState {
    fn new() -> Self {
        Self {
            access_token: None,
            config: ChatRuntimeConfig::default(),
            backend: ChatExecutionBackend::default(),
            sessions: HashMap::new(),
            latest_status: ChatRunStatus::default(),
            next_run_id: 1,
            external_tool_executor: ExternalToolExecutor::new(),
            subscribers: Vec::new(),
            sdk: None,
        }
    }

    fn get_or_create_session(&mut self, session_id: &str) -> &mut SessionState {
        self.sessions.entry(session_id.to_string()).or_default()
    }

    fn emit_event(&mut self, event: ChatRuntimeEvent) {
        self.subscribers
            .retain(|tx| tx.try_send(event.clone()).is_ok());
    }

    fn next_run_id(&mut self) -> u64 {
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        run_id
    }

    fn status_for_session(&self, session_id: &str) -> ChatRunStatus {
        match self.sessions.get(session_id) {
            Some(session) => ChatRunStatus {
                state: session.run_state,
                session_id: Some(session_id.to_string()),
                error_message: session.last_error.clone(),
                pending_interrupt: session.pending_interrupt.clone(),
            },
            None => ChatRunStatus {
                session_id: Some(session_id.to_string()),
                ..ChatRunStatus::default()
            },
        }
    }

    fn emit_session_status(&mut self, session_id: &str) {
        let status = self.status_for_session(session_id);
        self.latest_status = status.clone();
        self.emit_event(ChatRuntimeEvent::StatusChanged {
            state: status.state,
            session_id: status.session_id,
            error: status.error_message,
        });
    }

    fn status(&self) -> ChatRunStatus {
        self.latest_status.clone()
    }
}

fn build_execution_backend(config: &ChatRuntimeConfig) -> Result<ChatExecutionBackend, String> {
    build_chat_agent(config).map(ChatExecutionBackend)
}

fn normalize_active_agent_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    Some(match trimmed.to_ascii_lowercase().as_str() {
        "light" => "ask".to_string(),
        "deep" | "auto" => "manager".to_string(),
        other => other.to_string(),
    })
}

fn apply_active_agent_override(
    user_state: Option<JsonValue>,
    active_agent: Option<&str>,
) -> Option<JsonValue> {
    let Some(active_agent) = active_agent.and_then(normalize_active_agent_name) else {
        return user_state;
    };

    let mut user_state = match user_state {
        Some(JsonValue::Object(map)) => JsonValue::Object(map),
        _ => json!({}),
    };

    let Some(root) = user_state.as_object_mut() else {
        return Some(json!({
            "agent_mode": active_agent,
            "remi_handoff": { "current_agent": active_agent },
        }));
    };

    root.insert(
        "agent_mode".to_string(),
        JsonValue::String(active_agent.to_string()),
    );
    root.remove("handoff_summary");

    let handoff = root
        .entry("remi_handoff".to_string())
        .or_insert_with(|| json!({}));
    if !handoff.is_object() {
        *handoff = json!({});
    }
    if let Some(handoff) = handoff.as_object_mut() {
        handoff.insert(
            "current_agent".to_string(),
            JsonValue::String(active_agent.to_string()),
        );
        handoff.remove("handoff_summary");
    }

    Some(user_state)
}

fn parse_active_context_json_text(text: &str) -> Option<JsonValue> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parsed: JsonValue = serde_json::from_str(trimmed).ok()?;
    match &parsed {
        JsonValue::Object(map) if !map.is_empty() => Some(parsed),
        JsonValue::Array(_) => Some(parsed),
        _ => None,
    }
}

fn references_to_active_context_json(references: Option<&JsonValue>) -> Option<JsonValue> {
    let items = references?.as_array()?;
    if items.is_empty() {
        return None;
    }

    let mut modes: BTreeMap<String, Vec<JsonValue>> = BTreeMap::new();
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        let Some(item_type) = object.get("type").and_then(JsonValue::as_str) else {
            continue;
        };
        let Some(uuid) = object.get("uuid").and_then(JsonValue::as_str) else {
            continue;
        };

        let mode = object
            .get("mode")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("viewing")
            .to_string();

        let mut entry = serde_json::Map::new();
        entry.insert("type".to_string(), JsonValue::String(item_type.to_string()));
        entry.insert("uuid".to_string(), JsonValue::String(uuid.to_string()));
        if let Some(title) = object.get("title").and_then(JsonValue::as_str) {
            entry.insert("title".to_string(), JsonValue::String(title.to_string()));
        }
        if let Some(metadata) = object.get("metadata") {
            entry.insert("metadata".to_string(), metadata.clone());
        }
        if let Some(is_auto_reference) = object
            .get("is_auto_reference")
            .and_then(JsonValue::as_bool)
            .or_else(|| object.get("isAutoReference").and_then(JsonValue::as_bool))
        {
            entry.insert(
                "is_auto_reference".to_string(),
                JsonValue::Bool(is_auto_reference),
            );
        }

        modes
            .entry(mode)
            .or_default()
            .push(JsonValue::Object(entry));
    }

    if modes.is_empty() {
        return None;
    }

    Some(JsonValue::Object(
        modes
            .into_iter()
            .map(|(mode, entries)| (mode, JsonValue::Array(entries)))
            .collect(),
    ))
}

fn merge_active_context_into_user_state(
    user_state: Option<JsonValue>,
    active_context: Option<JsonValue>,
) -> Option<JsonValue> {
    let Some(active_context) = active_context else {
        return user_state;
    };

    let mut user_state = match user_state {
        Some(JsonValue::Object(map)) => JsonValue::Object(map),
        _ => json!({}),
    };

    if let Some(root) = user_state.as_object_mut() {
        root.insert("active_context".to_string(), active_context);
    }

    Some(user_state)
}

fn uploaded_images_to_user_state(image_uris: &[UploadedImage]) -> Option<JsonValue> {
    if image_uris.is_empty() {
        return None;
    }

    Some(json!({
        "images": image_uris
            .iter()
            .map(|image| json!({ "uri": image.remi_uri }))
            .collect::<Vec<_>>()
    }))
}

fn merge_chat_attachments_into_user_state(
    user_state: Option<JsonValue>,
    attachment_context: Option<JsonValue>,
) -> Option<JsonValue> {
    let Some(attachment_context) = attachment_context else {
        return user_state;
    };

    let mut user_state = match user_state {
        Some(JsonValue::Object(map)) => JsonValue::Object(map),
        _ => json!({}),
    };

    if let Some(root) = user_state.as_object_mut() {
        root.insert("chat_input_attachments".to_string(), attachment_context);
    }

    Some(user_state)
}

fn merge_configured_external_tool_names_into_user_state(
    user_state: Option<JsonValue>,
    configured_external_tool_names: Option<Vec<String>>,
) -> Option<JsonValue> {
    let configured_external_tool_names = configured_external_tool_names
        .unwrap_or_default()
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if configured_external_tool_names.is_empty() {
        return user_state;
    }

    let mut user_state = match user_state {
        Some(JsonValue::Object(map)) => JsonValue::Object(map),
        _ => json!({}),
    };

    if let Some(root) = user_state.as_object_mut() {
        root.insert(
            "configured_external_tool_names".to_string(),
            JsonValue::Array(
                configured_external_tool_names
                    .into_iter()
                    .map(JsonValue::String)
                    .collect(),
            ),
        );
    }

    Some(user_state)
}

fn build_chat_attachment_prompt(image_uris: &[UploadedImage]) -> Option<String> {
    if image_uris.is_empty() {
        return None;
    }

    let mut out = String::from(
        "## Chat Input Images\n\nThe current user message includes image attachments that are already available as remi:// URIs. If you want to save one into a Thing, call create_tool with type_name=\"image\", parent_path set to the target thing directory, and source_uri set to one of the URIs below. Example parent path: /collection/<collection_uuid>/things/<thing_uuid>.\n\n```yaml\nimages:\n",
    );
    for image in image_uris {
        out.push_str(&format!(
            "  - uri: \"{}\"\n",
            image.remi_uri.replace('"', "\\\"")
        ));
    }
    out.push_str("```");
    Some(out)
}

fn build_transient_system_prompt(
    sdk: Option<&Arc<TriggerSdk>>,
    device_id: &str,
    system_prompt: Option<&str>,
    active_context_json: Option<&JsonValue>,
    attachment_prompt: Option<&str>,
) -> Option<String> {
    let system_prompt = system_prompt.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    });

    let system_prompt_active_context = system_prompt
        .as_deref()
        .and_then(parse_active_context_json_text);

    let active_context = active_context_json
        .cloned()
        .or(system_prompt_active_context.clone());

    let active_context_prompt = active_context
        .as_ref()
        .and_then(|value| serde_json::to_string(value).ok())
        .and_then(|raw| {
            sdk.and_then(|sdk| {
                sdk.build_active_context_prompt(device_id, Some(&raw))
                    .ok()
                    .flatten()
            })
        });

    let base_prompt = match (system_prompt, active_context_prompt) {
        (Some(_existing), Some(active_prompt)) if system_prompt_active_context.is_some() => {
            Some(active_prompt)
        }
        (Some(existing), Some(_active_prompt)) => Some(existing),
        (Some(existing), None) => Some(existing),
        (None, Some(active_prompt)) => Some(active_prompt),
        (None, None) => None,
    };

    match (
        base_prompt,
        attachment_prompt.filter(|value| !value.trim().is_empty()),
    ) {
        (Some(base), Some(attachment_prompt)) => Some(format!("{base}\n\n{attachment_prompt}")),
        (Some(base), None) => Some(base),
        (None, Some(attachment_prompt)) => Some(attachment_prompt.to_string()),
        (None, None) => None,
    }
}

async fn open_chat_stream(
    backend: &ChatExecutionBackend,
    access_token: &str,
    config: &ChatRuntimeConfig,
    session_id: String,
    start_input: Option<chat_proto::ChatStartInput>,
    resume_input: Option<chat_proto::ChatResumeInput>,
) -> Result<ChatEventStream, String> {
    tracing::debug!(
        session_id = %session_id,
        backend = backend.0.backend_name(),
        "[ChatRuntime] opening chat stream via shared chat agent"
    );
    backend
        .0
        .open_stream(access_token, config, session_id, start_input, resume_input)
        .await
}

async fn ensure_protocol_state_loaded(
    state: &Arc<RwLock<ActorState>>,
    session_id: &str,
) -> Result<(), String> {
    let (needs_load, needs_message_hydration, sdk) = {
        let s = state.read().await;
        let (needs_load, needs_message_hydration) = s
            .sessions
            .get(session_id)
            .map(|session| (!session.protocol_loaded, session.messages.is_empty()))
            .unwrap_or((true, true));
        (needs_load, needs_message_hydration, s.sdk.clone())
    };

    if !needs_load && !needs_message_hydration {
        return Ok(());
    }

    let (stored_state, stored_messages) = match sdk {
        Some(sdk) => {
            let stored_state = if needs_load {
                sdk.get_chat_runtime_state_json(session_id)
                    .map_err(|e| format!("Failed to load chat runtime state: {}", e))?
            } else {
                None
            };

            let stored_messages = if needs_message_hydration {
                sdk.list_chat_messages_json(session_id, None, 0)
                    .map_err(|e| format!("Failed to load chat messages: {}", e))?
                    .into_iter()
                    .map(|json| {
                        let mut message = serde_json::from_str::<CachedMessage>(&json)
                            .map_err(|e| format!("Failed to parse cached chat message: {}", e))?;
                        message.refresh_ui_elements();
                        Ok(message)
                    })
                    .collect::<Result<Vec<_>, String>>()?
            } else {
                Vec::new()
            };

            (stored_state, stored_messages)
        }
        None => (None, Vec::new()),
    };

    let loaded_state = match stored_state {
        Some(json) => serde_json::from_str::<ChatProtocolSessionState>(&json)
            .map_err(|e| format!("Failed to parse chat runtime state: {}", e))?,
        None => ChatProtocolSessionState::default(),
    };

    let mut s = state.write().await;
    let session = s.get_or_create_session(session_id);
    if !session.protocol_loaded {
        session.protocol_state = loaded_state;
        session.protocol_loaded = true;
    }
    if session.messages.is_empty() && !stored_messages.is_empty() {
        session.index.clear();
        session.sub_sessions.clear();
        for message in stored_messages {
            if let Some(sub_session) = message.sub_session.clone() {
                session.sub_sessions.insert(
                    sub_session.parent_tool_call_id.clone(),
                    ProtocolSubSessionDraft::from_cached(sub_session),
                );
            }
            let id = message.id.clone();
            let idx = session.messages.len();
            session.messages.push(message);
            session.index.insert(id, idx);
        }
        session.version += 1;
    }

    Ok(())
}

fn content_to_proto_parts(content: &JsonValue) -> (String, Vec<chat_proto::ChatContentPart>) {
    match content {
        JsonValue::String(text) => (text.clone(), vec![]),
        JsonValue::Array(parts) => {
            let mut text_content = String::new();
            let mut proto_parts = Vec::new();

            for part in parts {
                let Some(part_type) = part.get("type").and_then(|value| value.as_str()) else {
                    continue;
                };

                match part_type {
                    "text" => {
                        let text = part
                            .get("text")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string();
                        text_content.push_str(&text);
                        proto_parts.push(chat_proto::ChatContentPart {
                            r#type: "text".to_string(),
                            value: Some(chat_proto::chat_content_part::Value::Text(text)),
                        });
                    }
                    "image_url" => {
                        let image = part.get("image_url").cloned().unwrap_or(JsonValue::Null);
                        let url = image
                            .get("url")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let detail = image
                            .get("detail")
                            .and_then(|value| value.as_str())
                            .unwrap_or("auto")
                            .to_string();
                        proto_parts.push(chat_proto::ChatContentPart {
                            r#type: "image_url".to_string(),
                            value: Some(chat_proto::chat_content_part::Value::ImageUrl(
                                chat_proto::ChatImageContent {
                                    url,
                                    detail,
                                    data: Vec::new(),
                                    media_type: String::new(),
                                },
                            )),
                        });
                    }
                    "resource_uri" => {
                        let uri = part
                            .get("uri")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let resource_type = part
                            .get("resource_type")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_string();
                        proto_parts.push(chat_proto::ChatContentPart {
                            r#type: "resource_uri".to_string(),
                            value: Some(chat_proto::chat_content_part::Value::ResourceUri(
                                chat_proto::ChatResourceContent { uri, resource_type },
                            )),
                        });
                    }
                    _ => {}
                }
            }

            (text_content, proto_parts)
        }
        _ => (String::new(), vec![]),
    }
}

fn protocol_history_to_proto(message: &ProtocolHistoryMessage) -> chat_proto::ChatHistoryMessage {
    let (content, content_parts) = content_to_proto_parts(&message.content);

    chat_proto::ChatHistoryMessage {
        id: message.id.clone(),
        role: message.role.clone(),
        content,
        content_parts,
        tool_calls: message
            .tool_calls
            .iter()
            .map(|tool_call| chat_proto::ChatToolCall {
                id: tool_call.id.clone(),
                tool_name: tool_call.tool_name.clone(),
                arguments: match &tool_call.arguments {
                    JsonValue::Object(_) => {
                        Some(json_value_to_prost_struct(tool_call.arguments.clone()))
                    }
                    _ => None,
                },
            })
            .collect(),
        tool_call_id: message.tool_call_id.clone().unwrap_or_default(),
        reasoning_content: message.reasoning_content.clone().unwrap_or_default(),
    }
}

fn tool_outcome_to_proto(
    outcome: &ToolExecutionOutcome,
) -> Option<chat_proto::ChatToolCallOutcome> {
    let outcome_value = if let Some(parts) = &outcome.result_parts {
        // Multimodal: encode each image part into proto ChatContentPart
        let proto_parts = parts
            .iter()
            .flat_map(|img| {
                let mut content_parts = Vec::new();
                if let Some(text) = image_part_exif_text(img.exif.as_ref()) {
                    content_parts.push(chat_proto::ChatContentPart {
                        r#type: "text".to_string(),
                        value: Some(chat_proto::chat_content_part::Value::Text(text)),
                    });
                }
                content_parts.push(chat_proto::ChatContentPart {
                    r#type: "image".to_string(),
                    value: Some(chat_proto::chat_content_part::Value::ImageUrl(
                        chat_proto::ChatImageContent {
                            url: String::new(),
                            detail: String::new(),
                            data: img.data.clone(),
                            media_type: img.media_type.clone(),
                        },
                    )),
                });
                content_parts
            })
            .collect();
        Some(chat_proto::chat_tool_call_outcome::Outcome::Parts(
            chat_proto::ChatToolCallResultParts { parts: proto_parts },
        ))
    } else if let Some(result) = &outcome.result {
        Some(chat_proto::chat_tool_call_outcome::Outcome::Result(
            result.clone(),
        ))
    } else {
        outcome
            .error
            .as_ref()
            .map(|error| chat_proto::chat_tool_call_outcome::Outcome::Error(error.clone()))
    }?;

    Some(chat_proto::ChatToolCallOutcome {
        tool_call_id: outcome.tool_call_id.clone(),
        tool_name: outcome.tool_name.clone(),
        outcome: Some(outcome_value),
    })
}

fn proto_tool_outcome_to_runtime(
    outcome: &chat_proto::ChatToolCallOutcome,
) -> Option<ToolExecutionOutcome> {
    let outcome_value = outcome.outcome.as_ref()?;
    match outcome_value {
        chat_proto::chat_tool_call_outcome::Outcome::Result(result) => Some(ToolExecutionOutcome {
            tool_call_id: outcome.tool_call_id.clone(),
            tool_name: outcome.tool_name.clone(),
            result: Some(result.clone()),
            result_parts: None,
            error: None,
            duration_ms: None,
        }),
        chat_proto::chat_tool_call_outcome::Outcome::Parts(parts_msg) => {
            let mut pending_exif = None;
            let mut text_parts = Vec::new();
            let images = parts_msg
                .parts
                .iter()
                .filter_map(|part| match &part.value {
                    Some(chat_proto::chat_content_part::Value::Text(text)) => {
                        if let Some(exif) = parse_image_exif_text(text) {
                            pending_exif = Some(exif);
                        } else {
                            text_parts.push(text.clone());
                        }
                        None
                    }
                    Some(chat_proto::chat_content_part::Value::ImageUrl(img))
                        if !img.data.is_empty() =>
                    {
                        Some(crate::chat_types::ToolImagePart {
                            media_type: img.media_type.clone(),
                            data: img.data.clone(),
                            exif: pending_exif.take(),
                        })
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let text_result = (!text_parts.is_empty()).then(|| text_parts.join("\n"));
            Some(ToolExecutionOutcome {
                tool_call_id: outcome.tool_call_id.clone(),
                tool_name: outcome.tool_name.clone(),
                result: if images.is_empty() { text_result } else { None },
                result_parts: if images.is_empty() {
                    None
                } else {
                    Some(images)
                },
                error: None,
                duration_ms: None,
            })
        }
        chat_proto::chat_tool_call_outcome::Outcome::Error(error) => Some(ToolExecutionOutcome {
            tool_call_id: outcome.tool_call_id.clone(),
            tool_name: outcome.tool_name.clone(),
            result: None,
            result_parts: None,
            error: Some(error.clone()),
            duration_ms: None,
        }),
    }
}

fn parse_tool_arguments_json(arguments_json: &str) -> JsonValue {
    if arguments_json.trim().is_empty() {
        return json!({});
    }

    serde_json::from_str(arguments_json)
        .unwrap_or_else(|_| JsonValue::String(arguments_json.to_string()))
}

fn outcome_to_protocol_tool_message(outcome: &ToolExecutionOutcome) -> ProtocolHistoryMessage {
    // Build content: for multimodal (image parts) use a JSON array of content parts
    // so that content_to_proto_parts() can later convert them for history replay.
    let content = if let Some(parts) = &outcome.result_parts {
        let parts_json: Vec<JsonValue> = parts
            .iter()
            .flat_map(|img| {
                let mut content_parts = Vec::new();
                if let Some(text) = image_part_exif_text(img.exif.as_ref()) {
                    content_parts.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }
                let b64 = base64::engine::general_purpose::STANDARD.encode(&img.data);
                content_parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{}", img.media_type, b64)
                    }
                }));
                content_parts
            })
            .collect();
        JsonValue::Array(parts_json)
    } else {
        JsonValue::String(
            outcome
                .result
                .clone()
                .or_else(|| outcome.error.clone())
                .unwrap_or_default(),
        )
    };

    ProtocolHistoryMessage {
        id: format!("tool:{}", outcome.tool_call_id),
        role: "tool".to_string(),
        content,
        tool_calls: Vec::new(),
        tool_call_id: Some(outcome.tool_call_id.clone()),
        reasoning_content: None,
    }
}

const IMAGE_EXIF_TEXT_PREFIX: &str = "Image EXIF:\n";

fn image_part_exif_text(exif: Option<&JsonValue>) -> Option<String> {
    let exif = exif?;
    let json = serde_json::to_string_pretty(exif)
        .or_else(|_| serde_json::to_string(exif))
        .ok()?;
    Some(format!("{IMAGE_EXIF_TEXT_PREFIX}{json}"))
}

fn parse_image_exif_text(text: &str) -> Option<JsonValue> {
    let json = text.strip_prefix(IMAGE_EXIF_TEXT_PREFIX)?;
    serde_json::from_str(json).ok()
}

fn cached_message_storage_json(session_id: &str, message: &CachedMessage) -> String {
    let mut value = serde_json::to_value(message).unwrap_or_else(|_| json!({}));
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "session_id".to_string(),
            JsonValue::String(session_id.to_string()),
        );
        let created_at =
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(message.timestamp_ms)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
        obj.insert("created_at".to_string(), JsonValue::String(created_at));
        obj.insert(
            "ui_elements".to_string(),
            serde_json::to_value(&message.ui_elements).unwrap_or(JsonValue::Array(Vec::new())),
        );
    }
    value.to_string()
}

// ═══════════════════════════════════════════════════════════════════════════════
// ChatRuntime Handle
// ═══════════════════════════════════════════════════════════════════════════════

static STATUS_VERSION: AtomicU64 = AtomicU64::new(0);

/// Handle to the chat runtime actor
#[derive(Clone)]
pub struct ChatRuntime {
    cmd_tx: mpsc::Sender<Command>,
}

impl ChatRuntime {
    /// Start the chat runtime actor
    pub fn start(sdk: Arc<TriggerSdk>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        tokio::spawn(run_actor(cmd_rx, sdk));
        Self { cmd_tx }
    }

    /// Initialize the runtime with configuration
    pub async fn init(
        &self,
        access_token: String,
        config: ChatRuntimeConfig,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Init {
                access_token,
                config,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "Runtime stopped")?;
        reply_rx.await.map_err(|_| "Runtime stopped")?
    }

    /// Update access token
    pub async fn update_token(&self, access_token: String) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::UpdateToken {
                access_token,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "Runtime stopped")?;
        reply_rx.await.map_err(|_| "Runtime stopped")?;
        Ok(())
    }

    /// Send a message and start streaming
    pub async fn send_message(
        &self,
        session_id: String,
        message: String,
        system_prompt: Option<String>,
        references: Option<JsonValue>,
        attachments: Option<JsonValue>,
        active_agent: Option<String>,
        configured_external_tool_names: Option<Vec<String>>,
        user_msg_id: Option<String>,
        assistant_msg_id: Option<String>,
    ) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::SendMessage {
                session_id,
                message,
                system_prompt,
                references,
                attachments,
                active_agent,
                configured_external_tool_names,
                user_msg_id,
                assistant_msg_id,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "Runtime stopped")?;
        reply_rx.await.map_err(|_| "Runtime stopped")?
    }

    /// Resume after waiting for user
    pub async fn resume(&self, session_id: String, resume_value: JsonValue) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Resume {
                session_id,
                resume_value,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "Runtime stopped")?;
        reply_rx.await.map_err(|_| "Runtime stopped")?
    }

    /// Cancel one session or, when omitted, all running sessions.
    pub async fn cancel(&self, session_id: Option<String>) -> Result<(), String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Cancel {
                session_id,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "Runtime stopped")?;
        reply_rx.await.map_err(|_| "Runtime stopped")?;
        Ok(())
    }

    /// Get current status
    pub async fn get_status(&self) -> ChatRunStatus {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Command::GetStatus { reply: reply_tx })
            .await
            .is_err()
        {
            return ChatRunStatus::default();
        }
        reply_rx.await.unwrap_or_default()
    }

    /// Get status for one session.
    pub async fn get_session_status(&self, session_id: &str) -> ChatRunStatus {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Command::GetSessionStatus {
                session_id: session_id.to_string(),
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return ChatRunStatus {
                session_id: Some(session_id.to_string()),
                ..ChatRunStatus::default()
            };
        }
        reply_rx.await.unwrap_or(ChatRunStatus {
            session_id: Some(session_id.to_string()),
            ..ChatRunStatus::default()
        })
    }

    /// Get status version (for polling-based change detection)
    pub fn get_status_version() -> u64 {
        STATUS_VERSION.load(Ordering::SeqCst)
    }

    /// Get cached messages for a session
    pub async fn get_messages(&self, session_id: &str) -> Vec<CachedMessage> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Command::GetMessages {
                session_id: session_id.to_string(),
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }

    /// Get cache version for a session
    pub async fn get_cache_version(&self, session_id: &str) -> u64 {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Command::GetCacheVersion {
                session_id: session_id.to_string(),
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return 0;
        }
        reply_rx.await.unwrap_or(0)
    }

    /// Initialize cache from existing messages
    pub async fn init_cache(&self, session_id: &str, messages: Vec<CachedMessage>) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Command::InitCache {
                session_id: session_id.to_string(),
                messages,
                reply: reply_tx,
            })
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }

    /// Clear session cache
    pub async fn clear_cache(&self, session_id: &str) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Command::ClearCache {
                session_id: session_id.to_string(),
                reply: reply_tx,
            })
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }

    /// Set the unified external tool executor.
    pub async fn set_external_tool_executor(&self, executor: ExternalToolExecutor) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Command::SetExternalToolExecutor {
                executor,
                reply: reply_tx,
            })
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }

    /// Subscribe to runtime events
    pub async fn subscribe(&self) -> mpsc::Receiver<ChatRuntimeEvent> {
        let (tx, rx) = mpsc::channel(64);
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Command::Subscribe {
                tx,
                reply: reply_tx,
            })
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
        rx
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Actor Loop
// ═══════════════════════════════════════════════════════════════════════════════

async fn run_actor(mut cmd_rx: mpsc::Receiver<Command>, sdk: Arc<TriggerSdk>) {
    let state = Arc::new(RwLock::new(ActorState::new()));
    state.write().await.sdk = Some(sdk);

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            Command::Init {
                access_token,
                config,
                reply,
            } => {
                let backend = match build_execution_backend(&config) {
                    Ok(backend) => backend,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                };
                let mut s = state.write().await;
                s.access_token = Some(access_token);
                s.config = config;
                s.backend = backend;
                let _ = reply.send(Ok(()));
            }

            Command::UpdateToken {
                access_token,
                reply,
            } => {
                state.write().await.access_token = Some(access_token);
                let _ = reply.send(());
            }

            Command::SendMessage {
                session_id,
                message,
                system_prompt,
                references,
                attachments,
                active_agent,
                configured_external_tool_names,
                user_msg_id,
                assistant_msg_id,
                reply,
            } => {
                let result = handle_send_message(
                    state.clone(),
                    session_id,
                    message,
                    system_prompt,
                    references,
                    attachments,
                    active_agent,
                    configured_external_tool_names,
                    user_msg_id,
                    assistant_msg_id,
                )
                .await;
                let _ = reply.send(result);
            }

            Command::Resume {
                session_id,
                resume_value,
                reply,
            } => {
                let result = handle_resume(state.clone(), session_id, resume_value).await;
                let _ = reply.send(result);
            }

            Command::Cancel { session_id, reply } => {
                let mut cancellations = Vec::new();
                {
                    let mut s = state.write().await;
                    match session_id.as_ref() {
                        Some(session_id) => {
                            if let Some(session) = s.sessions.get_mut(session_id) {
                                if let Some(tx) = session.cancel_tx.take() {
                                    cancellations.push(tx);
                                }
                                session.current_run_id = None;
                                session.run_state = ChatRunState::Idle;
                                session.last_error = None;
                                session.pending_interrupt = None;
                                s.emit_session_status(session_id);
                            }
                        }
                        None => {
                            let session_ids = s.sessions.keys().cloned().collect::<Vec<_>>();
                            for session_id in session_ids {
                                if let Some(session) = s.sessions.get_mut(&session_id) {
                                    if let Some(tx) = session.cancel_tx.take() {
                                        cancellations.push(tx);
                                    }
                                    session.current_run_id = None;
                                    session.run_state = ChatRunState::Idle;
                                    session.last_error = None;
                                    session.pending_interrupt = None;
                                }
                                s.emit_session_status(&session_id);
                            }
                        }
                    }
                }
                for tx in cancellations {
                    let _ = tx.send(()).await;
                }
                STATUS_VERSION.fetch_add(1, Ordering::SeqCst);
                let _ = reply.send(());
            }

            Command::GetStatus { reply } => {
                let s = state.read().await;
                let _ = reply.send(s.status());
            }

            Command::GetSessionStatus { session_id, reply } => {
                let s = state.read().await;
                let _ = reply.send(s.status_for_session(&session_id));
            }

            Command::GetMessages { session_id, reply } => {
                let s = state.read().await;
                let msgs = s
                    .sessions
                    .get(&session_id)
                    .map(|sess| sess.get_messages())
                    .unwrap_or_default();
                let _ = reply.send(msgs);
            }

            Command::GetCacheVersion { session_id, reply } => {
                let s = state.read().await;
                let v = s
                    .sessions
                    .get(&session_id)
                    .map(|sess| sess.version)
                    .unwrap_or(0);
                let _ = reply.send(v);
            }

            Command::InitCache {
                session_id,
                messages,
                reply,
            } => {
                let mut s = state.write().await;
                let sess = s.get_or_create_session(&session_id);
                sess.messages.clear();
                sess.index.clear();
                sess.sub_sessions.clear();
                for mut msg in messages {
                    msg.refresh_ui_elements();
                    if let Some(sub_session) = msg.sub_session.clone() {
                        sess.sub_sessions.insert(
                            sub_session.parent_tool_call_id.clone(),
                            ProtocolSubSessionDraft::from_cached(sub_session),
                        );
                    }
                    let id = msg.id.clone();
                    let idx = sess.messages.len();
                    sess.messages.push(msg);
                    sess.index.insert(id, idx);
                }
                sess.version += 1;
                let _ = reply.send(());
            }

            Command::ClearCache { session_id, reply } => {
                state.write().await.sessions.remove(&session_id);
                let _ = reply.send(());
            }

            Command::SetExternalToolExecutor { executor, reply } => {
                state.write().await.external_tool_executor = executor;
                let _ = reply.send(());
            }

            Command::Subscribe { tx, reply } => {
                state.write().await.subscribers.push(tx);
                let _ = reply.send(());
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Multimodal Message Builder
// ═══════════════════════════════════════════════════════════════════════════════

/// Uploaded image info for building resource_uri content parts.
struct UploadedImage {
    /// remi:// URI pointing to the uploaded image
    remi_uri: String,
}

/// Upload chat image attachments to cloud storage and return remi:// URIs.
///
/// For each image attachment that has a local `path`, reads the file bytes,
/// uploads via the global ProfileClient with scenario="chat", and constructs
/// a `remi://remote/...` URI. Images that already have a `url` starting with
/// `remi://` are passed through as-is.
///
/// Returns an error if any image fails to read or upload — the caller should
/// abort the chat send so users are not silently losing their images.
async fn upload_chat_image_attachments(
    attachments: &Option<JsonValue>,
) -> Result<Vec<UploadedImage>, String> {
    let Some(JsonValue::Array(arr)) = attachments else {
        return Ok(vec![]);
    };

    let mut uploaded = Vec::new();

    for item in arr {
        let type_field = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if type_field != "image" {
            continue;
        }

        // If attachment already has a remi:// URI, use it directly
        if let Some(url) = item.get("url").and_then(|v| v.as_str()) {
            if url.starts_with("remi://") {
                uploaded.push(UploadedImage {
                    remi_uri: url.to_string(),
                });
                continue;
            }
        }

        // Get local file path
        let path = match item.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.trim().is_empty() => p,
            _ => continue,
        };

        // Read file bytes
        let bytes = match tokio::fs::read(path).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(path = %path, error = %e, "Failed to read image file for chat upload");
                return Err(format!("Failed to read image file: {}", e));
            }
        };

        // Determine extension
        let ext = path.rsplit('.').next().unwrap_or("jpg").to_lowercase();

        // Upload via ProfileClient with scenario="chat"
        match crate::profile::media_upload(bytes, ext.clone(), Some("chat".to_string())).await {
            Ok(public_url) => {
                // Construct remi://remote URI
                let mime = crate::remi_uri::mime_from_extension(&ext);
                let remi = crate::remi_uri::RemiUri::from_remote(&public_url, &mime);
                uploaded.push(UploadedImage {
                    remi_uri: remi.to_uri_string(),
                });
                tracing::info!(
                    path = %path,
                    remi_uri = %remi.to_uri_string(),
                    "Chat image uploaded successfully"
                );
            }
            Err(e) => {
                tracing::error!(path = %path, error = %e, "Failed to upload chat image");
                return Err(format!("Failed to upload image: {}", e));
            }
        }
    }

    Ok(uploaded)
}

async fn prepare_chat_image_attachments(
    config: &ChatRuntimeConfig,
    attachments: &Option<JsonValue>,
) -> Result<Vec<UploadedImage>, String> {
    match &config.backend {
        ChatRuntimeBackend::RemoteServer => upload_chat_image_attachments(attachments).await,
        ChatRuntimeBackend::LocalWasm(_) => {
            prepare_local_chat_image_attachments(&config.device_id, attachments)
        }
    }
}

fn prepare_local_chat_image_attachments(
    device_id: &str,
    attachments: &Option<JsonValue>,
) -> Result<Vec<UploadedImage>, String> {
    let Some(JsonValue::Array(arr)) = attachments else {
        return Ok(vec![]);
    };

    let mut images = Vec::new();

    for item in arr {
        let type_field = item
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if type_field != "image" {
            continue;
        }

        if let Some(url) = item.get("url").and_then(|value| value.as_str()) {
            if !url.trim().is_empty() {
                images.push(UploadedImage {
                    remi_uri: url.to_string(),
                });
                continue;
            }
        }

        let Some(path) = item.get("path").and_then(|value| value.as_str()) else {
            continue;
        };
        if path.trim().is_empty() {
            continue;
        }

        let mime = item
            .get("mime")
            .or_else(|| item.get("mime_type"))
            .or_else(|| item.get("contentType"))
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .unwrap_or_else(|| {
                let ext = path.rsplit('.').next().unwrap_or("jpg");
                mime_from_extension(ext).to_string()
            });

        let remi = RemiUri::from_local_file(path, &mime, device_id);
        images.push(UploadedImage {
            remi_uri: remi.to_uri_string(),
        });
    }

    Ok(images)
}

/// Build the current user input from text + uploaded image URIs.
///
/// When `image_uris` is non-empty, the message is constructed with
/// `content_parts` containing text and `resource_uri` parts. The server will
/// resolve these URIs before forwarding to the model.
fn build_current_input_message(
    message: &str,
    image_uris: &[UploadedImage],
) -> chat_proto::ChatInputMessage {
    if image_uris.is_empty() {
        return chat_proto::ChatInputMessage {
            role: "user".to_string(),
            content: message.to_string(),
            content_parts: vec![],
        };
    }

    let mut parts = Vec::new();

    if !message.trim().is_empty() {
        parts.push(chat_proto::ChatContentPart {
            r#type: "text".to_string(),
            value: Some(chat_proto::chat_content_part::Value::Text(
                message.to_string(),
            )),
        });
    }

    for img in image_uris {
        parts.push(chat_proto::ChatContentPart {
            r#type: "resource_uri".to_string(),
            value: Some(chat_proto::chat_content_part::Value::ResourceUri(
                chat_proto::ChatResourceContent {
                    uri: img.remi_uri.clone(),
                    resource_type: "image".to_string(),
                },
            )),
        });
    }

    chat_proto::ChatInputMessage {
        role: "user".to_string(),
        content: message.to_string(),
        content_parts: parts,
    }
}

fn input_message_to_protocol_history(
    id: String,
    input: &chat_proto::ChatInputMessage,
) -> ProtocolHistoryMessage {
    let content = if input.content_parts.is_empty() {
        JsonValue::String(input.content.clone())
    } else {
        JsonValue::Array(
            input
                .content_parts
                .iter()
                .map(|part| match part.value.as_ref() {
                    Some(chat_proto::chat_content_part::Value::Text(text)) => json!({
                        "type": "text",
                        "text": text,
                    }),
                    Some(chat_proto::chat_content_part::Value::ImageUrl(image)) => json!({
                        "type": "image_url",
                        "image_url": {
                            "url": image.url,
                            "detail": image.detail,
                        },
                    }),
                    Some(chat_proto::chat_content_part::Value::ResourceUri(resource)) => json!({
                        "type": "resource_uri",
                        "uri": resource.uri,
                        "resource_type": resource.resource_type,
                    }),
                    None => json!({
                        "type": part.r#type,
                    }),
                })
                .collect(),
        )
    };

    ProtocolHistoryMessage {
        id,
        role: if input.role.trim().is_empty() {
            "user".to_string()
        } else {
            input.role.clone()
        },
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        reasoning_content: None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Send Message Handler
// ═══════════════════════════════════════════════════════════════════════════════

async fn handle_send_message(
    state: Arc<RwLock<ActorState>>,
    session_id: String,
    message: String,
    system_prompt: Option<String>,
    references: Option<JsonValue>,
    attachments: Option<JsonValue>,
    active_agent: Option<String>,
    configured_external_tool_names: Option<Vec<String>>,
    user_msg_id: Option<String>,
    assistant_msg_id: Option<String>,
) -> Result<(), String> {
    tracing::info!(
        session_id = %session_id,
        msg_len = message.len(),
        "[ChatRuntime] handle_send_message enter"
    );
    // Get config
    let (access_token, config, backend, sdk) = {
        let s = state.read().await;
        (
            s.access_token.clone().ok_or("Not initialized")?,
            s.config.clone(),
            s.backend.clone(),
            s.sdk.clone(),
        )
    };

    ensure_protocol_state_loaded(&state, &session_id).await?;

    // Generate message IDs
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let user_id = user_msg_id.unwrap_or_else(|| format!("user_{}", now_ms));
    let assistant_id = assistant_msg_id.unwrap_or_else(|| format!("assistant_{}", now_ms));

    // Insert user message into cache.
    // NOTE: We intentionally do NOT pre-insert an assistant placeholder here.
    // The model may emit multiple assistant messages (different message IDs),
    // and pre-creating a single placeholder causes UI to look like it only
    // ever updates one bubble.
    let (cache_version, previous_cancel, run_id) = {
        let mut s = state.write().await;
        let run_id = s.next_run_id();
        let (version, previous_cancel) = {
            let sess = s.get_or_create_session(&session_id);
            let previous_cancel = sess.cancel_tx.take();
            sess.current_run_id = Some(run_id);
            sess.run_state = ChatRunState::Running;
            sess.last_error = None;
            sess.pending_interrupt = None;
            let mut m = CachedMessage::user(user_id.clone(), message.clone(), now_ms);
            m.references = references.clone();
            m.attachments = attachments.clone();
            sess.upsert(m);
            (sess.version, previous_cancel)
        };
        s.emit_session_status(&session_id);
        STATUS_VERSION.fetch_add(1, Ordering::SeqCst);
        s.emit_event(ChatRuntimeEvent::CacheUpdated {
            session_id: session_id.clone(),
            version,
        });
        (version, previous_cancel, run_id)
    };
    let _ = cache_version; // silence unused warning

    if let Some(tx) = previous_cancel {
        let _ = tx.send(()).await;
    }

    // Persist to DB
    if let Some(ref sdk) = sdk {
        let mut user_message = CachedMessage::user(user_id.clone(), message.clone(), now_ms);
        user_message.references = references.clone();
        user_message.attachments = attachments.clone();
        user_message.refresh_ui_elements();
        let _ = sdk.upsert_chat_message_json(
            session_id.clone(),
            user_id.clone(),
            now_ms,
            cached_message_storage_json(&session_id, &user_message),
        );

        // Ensure the session appears in history immediately (even while streaming).
        let title = {
            let content = message.trim();
            if content.len() > 50 {
                // Walk back from byte 47 to the nearest char boundary so we never
                // panic on multi-byte UTF-8 characters (e.g. CJK: 3 bytes each).
                let mut cut = 47.min(content.len());
                while !content.is_char_boundary(cut) {
                    cut -= 1;
                }
                Some(format!("{}...", &content[..cut]))
            } else if content.is_empty() {
                None
            } else {
                Some(content.to_string())
            }
        };
        let _ = sdk.upsert_chat_session(session_id.clone(), title, 1);
    }

    // Build the current input (with multimodal content_parts if images are attached)
    // Step 1: Upload any local images to cloud storage and get remi:// URIs.
    let image_uris = match prepare_chat_image_attachments(&config, &attachments).await {
        Ok(uris) => uris,
        Err(e) => {
            let error_msg = format!("Image upload failed: {}", e);
            tracing::error!(error = %error_msg, "Aborting chat send due to image upload failure");
            let mut s = state.write().await;
            if let Some(session) = s.sessions.get_mut(&session_id) {
                session.current_run_id = None;
                session.run_state = ChatRunState::Error;
                session.last_error = Some(error_msg.clone());
                session.pending_interrupt = None;
                session.cancel_tx = None;
            }
            s.emit_session_status(&session_id);
            return Err(error_msg);
        }
    };
    let current_input = build_current_input_message(&message, &image_uris);
    let pending_user = input_message_to_protocol_history(user_id.clone(), &current_input);
    let start_input = {
        let mut s = state.write().await;
        let session = s.get_or_create_session(&session_id);
        let latest_user_state = session
            .protocol_state
            .latest_state
            .as_ref()
            .and_then(|state| state.get("user_state"))
            .cloned();
        let raw_active_context =
            references_to_active_context_json(references.as_ref()).or_else(|| {
                system_prompt
                    .as_deref()
                    .and_then(parse_active_context_json_text)
            });
        let normalized_active_context = raw_active_context.as_ref().and_then(|value| {
            serde_json::to_string(value)
                .ok()
                .and_then(|raw| {
                    sdk.as_ref().and_then(|sdk| {
                        sdk.normalize_active_context_json(&config.device_id, Some(&raw))
                            .ok()
                            .flatten()
                    })
                })
                .or_else(|| Some(value.clone()))
        });
        let attachment_context = uploaded_images_to_user_state(&image_uris);
        let incoming_configured_external_tool_names = configured_external_tool_names.clone();
        let effective_user_state = merge_chat_attachments_into_user_state(
            merge_configured_external_tool_names_into_user_state(
                merge_active_context_into_user_state(
                    apply_active_agent_override(latest_user_state, active_agent.as_deref()),
                    normalized_active_context,
                ),
                configured_external_tool_names,
            ),
            attachment_context,
        );
        let attachment_prompt = build_chat_attachment_prompt(&image_uris);
        let transient_system_prompt = build_transient_system_prompt(
            sdk.as_ref(),
            &config.device_id,
            system_prompt.as_deref(),
            raw_active_context.as_ref(),
            attachment_prompt.as_deref(),
        );
        let effective_agent_mode = effective_user_state
            .as_ref()
            .and_then(|value| value.get("agent_mode"))
            .and_then(JsonValue::as_str)
            .map(ToString::to_string);
        let effective_configured_external_tool_names = effective_user_state
            .as_ref()
            .and_then(|value| value.get("configured_external_tool_names"))
            .and_then(JsonValue::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut request_history = session.protocol_state.history.clone();
        tracing::info!(
            session_id = %session_id,
            history_count = request_history.len(),
            history = ?request_history.iter().map(summarize_protocol_history_message).collect::<Vec<_>>(),
            "[ChatRuntime] building start input from protocol history"
        );
        if let Some(sys) = transient_system_prompt {
            request_history.push(ProtocolHistoryMessage {
                id: format!("system_{}", now_ms),
                role: "system".to_string(),
                content: JsonValue::String(sys),
                tool_calls: Vec::new(),
                tool_call_id: None,
                reasoning_content: None,
            });
        }

        let extra_tools =
            external_tool_schema::chat_start_extra_tools(effective_user_state.as_ref());
        tracing::info!(
            session_id = %session_id,
            incoming_active_agent = ?active_agent,
            incoming_configured_external_tool_names = ?incoming_configured_external_tool_names,
            effective_agent_mode = ?effective_agent_mode,
            effective_configured_external_tool_names = ?effective_configured_external_tool_names,
            extra_tool_count = extra_tools.len(),
            "[ChatRuntime] built start input extra tools"
        );

        chat_proto::ChatStartInput {
            history: request_history
                .iter()
                .map(protocol_history_to_proto)
                .collect(),
            current: Some(current_input.clone()),
            metadata: build_chat_start_metadata(&config, &session_id),
            extra_tools,
            user_state: effective_user_state.clone().map(json_value_to_prost_struct),
        }
    };

    // Create cancel channel
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);
    {
        let mut s = state.write().await;
        if let Some(session) = s.sessions.get_mut(&session_id) {
            if session.current_run_id == Some(run_id) {
                session.cancel_tx = Some(cancel_tx);
            }
        }
    }

    // Spawn stream processing task
    let state_clone = state.clone();
    let session_id_clone = session_id.clone();
    let assistant_id_clone = assistant_id.clone();
    let run_id_clone = run_id;

    tracing::info!(session_id = %session_id, "[ChatRuntime] spawning run_stream_loop");
    tokio::spawn(async move {
        tracing::info!(session_id = %session_id_clone, "[ChatRuntime] run_stream_loop task started");
        let result = run_stream_loop(
            state_clone.clone(),
            access_token,
            config,
            backend,
            session_id_clone.clone(),
            assistant_id_clone.clone(),
            ProtocolTurnState::for_user(pending_user),
            Some(start_input),
            None,
            cancel_rx,
        )
        .await;

        tracing::info!(
            session_id = %session_id_clone,
            ok = result.is_ok(),
            "[ChatRuntime] run_stream_loop finished"
        );

        // Update state based on result
        let mut should_persist = false;
        {
            let mut s = state_clone.write().await;
            let should_handle_result = s
                .sessions
                .get(&session_id_clone)
                .map(|session| session.current_run_id == Some(run_id_clone))
                .unwrap_or(false);
            if should_handle_result {
                match result {
                    Ok(StreamResult::Completed) => {
                        if let Some(session) = s.sessions.get_mut(&session_id_clone) {
                            session.current_run_id = None;
                            session.cancel_tx = None;
                            session.run_state = ChatRunState::Idle;
                            session.last_error = None;
                            session.pending_interrupt = None;
                        }
                        s.emit_session_status(&session_id_clone);
                        should_persist = true;
                    }
                    Ok(StreamResult::WaitingForUser(pending)) => {
                        if let Some(session) = s.sessions.get_mut(&session_id_clone) {
                            session.cancel_tx = None;
                            session.run_state = ChatRunState::WaitingForUser;
                            session.last_error = None;
                            session.pending_interrupt = Some(pending.clone());
                        }
                        let pending_tool_execution =
                            s.sessions.get(&session_id_clone).and_then(|session| {
                                session.protocol_state.pending_tool_execution.clone()
                            });
                        s.emit_session_status(&session_id_clone);
                        s.emit_event(ChatRuntimeEvent::InterruptPending {
                            session_id: session_id_clone.clone(),
                            interrupt_id: pending.interrupt_id,
                            interrupt_type: pending.interrupt_type,
                            display_data: pending.display_data,
                        });
                        if let Some(pending_tool_execution) = pending_tool_execution {
                            s.emit_event(ChatRuntimeEvent::NeedToolExecution {
                                session_id: session_id_clone.clone(),
                                pending_calls: pending_tool_execution.pending_calls.clone(),
                                pending_tool_execution,
                            });
                        }
                        should_persist = true;
                    }
                    Err(e) => {
                        if let Some(session) = s.sessions.get_mut(&session_id_clone) {
                            session.current_run_id = None;
                            session.cancel_tx = None;
                            session.run_state = ChatRunState::Error;
                            session.last_error = Some(e.clone());
                            session.pending_interrupt = None;
                        }
                        s.emit_session_status(&session_id_clone);

                        // Mark the last assistant message as error (best-effort).
                        let mut version_opt: Option<u64> = None;
                        if let Some(sess) = s.sessions.get_mut(&session_id_clone) {
                            let target_id = sess
                                .messages
                                .iter()
                                .rev()
                                .find(|m| !m.is_user)
                                .map(|m| m.id.clone())
                                .unwrap_or_else(|| assistant_id_clone.clone());

                            let mut m = CachedMessage::assistant(
                                target_id.clone(),
                                chrono::Utc::now().timestamp_millis(),
                            );
                            m.content = e;
                            m.has_error = Some(true);
                            sess.upsert(m);
                            version_opt = Some(sess.version);
                        }

                        if let Some(version) = version_opt {
                            s.emit_event(ChatRuntimeEvent::CacheUpdated {
                                session_id: session_id_clone.clone(),
                                version,
                            });
                        }

                        should_persist = true;
                    }
                }
                STATUS_VERSION.fetch_add(1, Ordering::SeqCst);
            }
        }

        if should_persist {
            persist_session(&state_clone, &session_id_clone).await;
        }
    });

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Resume Handler
// ═══════════════════════════════════════════════════════════════════════════════

async fn handle_resume(
    state: Arc<RwLock<ActorState>>,
    session_id: String,
    resume_value: JsonValue,
) -> Result<(), String> {
    ensure_protocol_state_loaded(&state, &session_id).await?;

    // Verify we're in WaitingForUser state
    let (access_token, config, backend, assistant_id, resume_input, previous_cancel, run_id) = {
        let mut s = state.write().await;
        let assistant_id = format!("assistant_{}", chrono::Utc::now().timestamp_millis());
        let run_id = s.next_run_id();

        let session = s
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "Session state missing".to_string())?;
        if session.run_state != ChatRunState::WaitingForUser {
            return Err("Not waiting for user".to_string());
        }
        let pending = session
            .protocol_state
            .pending_tool_execution
            .clone()
            .ok_or_else(|| "No pending tool execution state".to_string())?;
        let previous_cancel = session.cancel_tx.take();

        let manual_results = manual_resume_outcomes(&pending, resume_value)?;
        let mut all_results = Vec::new();
        all_results.extend(pending.completed_results.clone());
        all_results.extend(pending.resolved_results.clone());
        all_results.extend(manual_results);
        let normalized_state =
            external_tool_schema::normalize_resume_state_tool_definitions(pending.state.clone());

        for outcome in &all_results {
            session
                .protocol_state
                .history
                .push(outcome_to_protocol_tool_message(outcome));
        }
        session.protocol_state.pending_tool_execution = None;
        let resume_results =
            serialize_tool_outcomes_for_resume(&session_id, "manual_resume", &all_results);

        let resume_input = chat_proto::ChatResumeInput {
            state: Some(json_value_to_prost_struct(normalized_state)),
            results: resume_results,
        };

        session.current_run_id = Some(run_id);
        session.run_state = ChatRunState::Running;
        session.last_error = None;
        session.pending_interrupt = None;
        s.emit_session_status(&session_id);
        STATUS_VERSION.fetch_add(1, Ordering::SeqCst);

        (
            s.access_token.clone().ok_or("Not initialized")?,
            s.config.clone(),
            s.backend.clone(),
            assistant_id,
            resume_input,
            previous_cancel,
            run_id,
        )
    };

    if let Some(tx) = previous_cancel {
        let _ = tx.send(()).await;
    }

    // Create cancel channel
    let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);
    {
        let mut s = state.write().await;
        if let Some(session) = s.sessions.get_mut(&session_id) {
            if session.current_run_id == Some(run_id) {
                session.cancel_tx = Some(cancel_tx);
            }
        }
    }

    // Spawn stream processing task
    let state_clone = state.clone();
    let session_id_clone = session_id.clone();
    let assistant_id_clone = assistant_id.clone();
    let run_id_clone = run_id;

    tokio::spawn(async move {
        let result = run_stream_loop(
            state_clone.clone(),
            access_token,
            config,
            backend,
            session_id_clone.clone(),
            assistant_id_clone.clone(),
            ProtocolTurnState::default(),
            None,
            Some(resume_input),
            cancel_rx,
        )
        .await;

        // Update state based on result
        let mut should_persist = false;
        {
            let mut s = state_clone.write().await;
            let should_handle_result = s
                .sessions
                .get(&session_id_clone)
                .map(|session| session.current_run_id == Some(run_id_clone))
                .unwrap_or(false);
            if should_handle_result {
                match result {
                    Ok(StreamResult::Completed) => {
                        if let Some(session) = s.sessions.get_mut(&session_id_clone) {
                            session.current_run_id = None;
                            session.cancel_tx = None;
                            session.run_state = ChatRunState::Idle;
                            session.last_error = None;
                            session.pending_interrupt = None;
                        }
                        s.emit_session_status(&session_id_clone);
                        should_persist = true;
                    }
                    Ok(StreamResult::WaitingForUser(pending)) => {
                        if let Some(session) = s.sessions.get_mut(&session_id_clone) {
                            session.cancel_tx = None;
                            session.run_state = ChatRunState::WaitingForUser;
                            session.last_error = None;
                            session.pending_interrupt = Some(pending.clone());
                        }
                        let pending_tool_execution =
                            s.sessions.get(&session_id_clone).and_then(|session| {
                                session.protocol_state.pending_tool_execution.clone()
                            });
                        s.emit_session_status(&session_id_clone);
                        s.emit_event(ChatRuntimeEvent::InterruptPending {
                            session_id: session_id_clone.clone(),
                            interrupt_id: pending.interrupt_id,
                            interrupt_type: pending.interrupt_type,
                            display_data: pending.display_data,
                        });
                        if let Some(pending_tool_execution) = pending_tool_execution {
                            s.emit_event(ChatRuntimeEvent::NeedToolExecution {
                                session_id: session_id_clone.clone(),
                                pending_calls: pending_tool_execution.pending_calls.clone(),
                                pending_tool_execution,
                            });
                        }
                        should_persist = true;
                    }
                    Err(e) => {
                        if let Some(session) = s.sessions.get_mut(&session_id_clone) {
                            session.current_run_id = None;
                            session.cancel_tx = None;
                            session.run_state = ChatRunState::Error;
                            session.last_error = Some(e.clone());
                            session.pending_interrupt = None;
                        }
                        s.emit_session_status(&session_id_clone);

                        // Mark the last assistant message as error (best-effort).
                        let mut version_opt: Option<u64> = None;
                        if let Some(sess) = s.sessions.get_mut(&session_id_clone) {
                            let target_id = sess
                                .messages
                                .iter()
                                .rev()
                                .find(|m| !m.is_user)
                                .map(|m| m.id.clone())
                                .unwrap_or_else(|| assistant_id_clone.clone());

                            let mut m = CachedMessage::assistant(
                                target_id.clone(),
                                chrono::Utc::now().timestamp_millis(),
                            );
                            m.content = e;
                            m.has_error = Some(true);
                            sess.upsert(m);
                            version_opt = Some(sess.version);
                        }

                        if let Some(version) = version_opt {
                            s.emit_event(ChatRuntimeEvent::CacheUpdated {
                                session_id: session_id_clone.clone(),
                                version,
                            });
                        }

                        should_persist = true;
                    }
                }
                STATUS_VERSION.fetch_add(1, Ordering::SeqCst);
            }
        }

        if should_persist {
            persist_session(&state_clone, &session_id_clone).await;
        }
    });

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Stream Processing
// ═══════════════════════════════════════════════════════════════════════════════

enum StreamResult {
    Completed,
    WaitingForUser(PendingInterrupt),
}

enum StreamControl {
    Continue,
    AutoResume(chat_proto::ChatResumeInput),
    WaitForUser(PendingInterrupt),
    Cancelled,
}

async fn run_stream_loop(
    state: Arc<RwLock<ActorState>>,
    access_token: String,
    config: ChatRuntimeConfig,
    backend: ChatExecutionBackend,
    session_id: String,
    assistant_id: String,
    initial_turn_state: ProtocolTurnState,
    initial_start: Option<chat_proto::ChatStartInput>,
    initial_resume: Option<chat_proto::ChatResumeInput>,
    mut cancel_rx: mpsc::Receiver<()>,
) -> Result<StreamResult, String> {
    tracing::info!(session_id = %session_id, "[ChatRuntime] run_stream_loop enter");
    let mut start_input = initial_start;
    let mut resume_input = initial_resume;
    let mut next_turn_state = Some(initial_turn_state);
    let mut auto_resume_count = 0;
    let mut current_assistant_id = assistant_id;

    loop {
        let mut stream = open_chat_stream(
            &backend,
            &access_token,
            &config,
            session_id.clone(),
            start_input.take(),
            resume_input.take(),
        )
        .await?;
        tracing::info!(session_id = %session_id, "[ChatRuntime] stream started, processing chunks");

        let mut turn_state = next_turn_state.take().unwrap_or_default();

        let mut persist_tick = tokio::time::interval(std::time::Duration::from_millis(500));
        persist_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dirty_since_last_persist = false;
        let mut last_seen_version: Option<u64> = {
            let s = state.read().await;
            s.sessions.get(&session_id).map(|sess| sess.version)
        };

        loop {
            tokio::select! {
                _ = cancel_rx.recv() => {
                    tracing::info!(session_id, "Chat stream cancelled");
                    if dirty_since_last_persist {
                        persist_session(&state, &session_id).await;
                    }
                    return Ok(StreamResult::Completed);
                }
                _ = persist_tick.tick() => {
                    if dirty_since_last_persist {
                        persist_session(&state, &session_id).await;
                        dirty_since_last_persist = false;
                    }
                }
                chunk_result = stream.next() => {
                    match chunk_result {
                        None => {
                            if !turn_state.committed {
                                let mut s = state.write().await;
                                if let Some(session) = s.sessions.get_mut(&session_id) {
                                    turn_state.commit_completed(session, &current_assistant_id);
                                }
                            }
                            tracing::info!(session_id, "Chat stream completed");
                            persist_session(&state, &session_id).await;
                            return Ok(StreamResult::Completed);
                        }
                        Some(Err(e)) => {
                            return Err(format!("Stream error: {}", e));
                        }
                        Some(Ok(event)) => {
                            if let Some(fatal) = extract_fatal_error_from_event(&event) {
                                return Err(fatal);
                            }

                            if should_rotate_assistant_after_tool_phase(&turn_state, &event) {
                                let mut s = state.write().await;
                                if let Some(session) = s.sessions.get_mut(&session_id) {
                                    turn_state.flush_phase_into_history(session, &current_assistant_id);
                                }
                                current_assistant_id = format!(
                                    "assistant_{}_post_tool",
                                    chrono::Utc::now().timestamp_millis()
                                );
                            }

                            match process_stream_event(
                                &state,
                                &session_id,
                                &current_assistant_id,
                                &mut turn_state,
                                &event,
                            ).await? {
                                StreamControl::Continue => {}
                                StreamControl::AutoResume(resume) => {
                                    auto_resume_count += 1;
                                    if auto_resume_count > config.max_auto_resumes {
                                        return Err("Max auto-resume limit reached".to_string());
                                    }

                                    {
                                        let mut s = state.write().await;
                                        if let Some(session) = s.sessions.get_mut(&session_id) {
                                            turn_state.flush_phase_into_history(session, &current_assistant_id);
                                            tracing::info!(
                                                session_id = %session_id,
                                                assistant_id = %current_assistant_id,
                                                history_len = session.protocol_state.history.len(),
                                                "Flushed tool phase into protocol history before auto-resume"
                                            );
                                        }
                                    }

                                    tracing::info!(
                                        session_id,
                                        count = auto_resume_count,
                                        "[ChatRuntime] NeedToolExecution fully auto-resolved, resuming"
                                    );

                                    persist_session(&state, &session_id).await;
                                    resume_input = Some(resume);
                                    start_input = None;
                                    next_turn_state = Some(ProtocolTurnState::default());
                                    current_assistant_id = format!(
                                        "assistant_{}_{}",
                                        chrono::Utc::now().timestamp_millis(),
                                        auto_resume_count
                                    );
                                    break;
                                }
                                StreamControl::WaitForUser(pending) => {
                                    tracing::info!(
                                        interrupt_id = %pending.interrupt_id,
                                        interrupt_type = %pending.interrupt_type,
                                        "[ChatRuntime] Waiting for user to resolve external tool call"
                                    );
                                    persist_session(&state, &session_id).await;
                                    return Ok(StreamResult::WaitingForUser(pending));
                                }
                                StreamControl::Cancelled => {
                                    persist_session(&state, &session_id).await;
                                    return Ok(StreamResult::Completed);
                                }
                            }

                            let current_version: Option<u64> = {
                                let s = state.read().await;
                                s.sessions.get(&session_id).map(|sess| sess.version)
                            };
                            if current_version.is_some() && current_version != last_seen_version {
                                last_seen_version = current_version;
                                dirty_since_last_persist = true;
                            }
                        }
                    }
                }
            }
        }
    }
}

fn should_rotate_assistant_after_tool_phase(
    turn_state: &ProtocolTurnState,
    event: &ChatStreamEvent,
) -> bool {
    if !turn_state.has_tool_phase() {
        return false;
    }

    matches!(
        event.event.as_ref(),
        Some(chat_stream_event::Event::Delta(_))
            | Some(chat_stream_event::Event::ThinkingStart(_))
            | Some(chat_stream_event::Event::ThinkingEnd(_))
    )
}

async fn process_stream_event(
    state: &Arc<RwLock<ActorState>>,
    session_id: &str,
    assistant_id: &str,
    turn_state: &mut ProtocolTurnState,
    event: &ChatStreamEvent,
) -> Result<StreamControl, String> {
    match event.event.as_ref() {
        Some(chat_stream_event::Event::Delta(delta)) => {
            if delta.content.is_empty() {
                return Ok(StreamControl::Continue);
            }

            turn_state.record_delta(&delta.content);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let mut s = state.write().await;
            let version_opt = if let Some(session) = s.sessions.get_mut(session_id) {
                session.assistant_content.insert(
                    assistant_id.to_string(),
                    turn_state.assistant_content.clone(),
                );

                let mut message = CachedMessage::assistant(assistant_id.to_string(), now_ms);
                message.content = turn_state.assistant_content.clone();
                message.thinking = session.assistant_thinking.get(assistant_id).cloned();
                session.upsert(message);
                Some(session.version)
            } else {
                None
            };

            if let Some(version) = version_opt {
                s.emit_event(ChatRuntimeEvent::CacheUpdated {
                    session_id: session_id.to_string(),
                    version,
                });
            }
            Ok(StreamControl::Continue)
        }
        Some(chat_stream_event::Event::ThinkingStart(_)) => Ok(StreamControl::Continue),
        Some(chat_stream_event::Event::ThinkingEnd(thinking)) => {
            turn_state.record_reasoning(&thinking.content);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let mut s = state.write().await;
            let version_opt = if let Some(session) = s.sessions.get_mut(session_id) {
                session
                    .assistant_thinking
                    .insert(assistant_id.to_string(), thinking.content.clone());

                let mut message = CachedMessage::assistant(assistant_id.to_string(), now_ms);
                message.content = session
                    .assistant_content
                    .get(assistant_id)
                    .cloned()
                    .unwrap_or_default();
                message.thinking = Some(thinking.content.clone());
                session.upsert(message);
                Some(session.version)
            } else {
                None
            };

            if let Some(version) = version_opt {
                s.emit_event(ChatRuntimeEvent::CacheUpdated {
                    session_id: session_id.to_string(),
                    version,
                });
            }
            Ok(StreamControl::Continue)
        }
        Some(chat_stream_event::Event::ToolCallStart(tool_call)) => {
            turn_state.record_tool_call_start(&tool_call.id, &tool_call.tool_name);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let mut s = state.write().await;
            let version_opt = if let Some(session) = s.sessions.get_mut(session_id) {
                session
                    .tool_call_name
                    .insert(tool_call.id.clone(), tool_call.tool_name.clone());
                let message = build_tool_call_message(session, &tool_call.id, now_ms);
                session.upsert(message);
                Some(session.version)
            } else {
                None
            };

            if let Some(version) = version_opt {
                s.emit_event(ChatRuntimeEvent::CacheUpdated {
                    session_id: session_id.to_string(),
                    version,
                });
            }
            Ok(StreamControl::Continue)
        }
        Some(chat_stream_event::Event::ToolCallDelta(tool_call)) => {
            turn_state.record_tool_call_delta(&tool_call.id, &tool_call.arguments_delta);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let mut s = state.write().await;
            let version_opt = if let Some(session) = s.sessions.get_mut(session_id) {
                let entry = session
                    .tool_call_args
                    .entry(tool_call.id.clone())
                    .or_insert_with(BTreeMap::new);
                let combined = format!(
                    "{}{}",
                    entry.get(&0).cloned().unwrap_or_default(),
                    tool_call.arguments_delta
                );
                entry.insert(0, combined.clone());
                if !turn_state
                    .tool_calls
                    .iter()
                    .any(|call| call.id == tool_call.id)
                {
                    turn_state.record_tool_call_start(&tool_call.id, "(unknown)");
                }
                let message = build_tool_call_message(session, &tool_call.id, now_ms);
                session.upsert(message);
                Some(session.version)
            } else {
                None
            };

            if let Some(version) = version_opt {
                s.emit_event(ChatRuntimeEvent::CacheUpdated {
                    session_id: session_id.to_string(),
                    version,
                });
            }
            Ok(StreamControl::Continue)
        }
        Some(chat_stream_event::Event::ToolDelta(tool_delta)) => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let mut s = state.write().await;
            let version_opt = if let Some(session) = s.sessions.get_mut(session_id) {
                if !tool_delta.tool_name.trim().is_empty() {
                    session
                        .tool_call_name
                        .insert(tool_delta.id.clone(), tool_delta.tool_name.clone());
                }

                let combined = {
                    let entry = session
                        .tool_result_content
                        .entry(tool_delta.id.clone())
                        .or_insert_with(String::new);
                    entry.push_str(&tool_delta.delta);
                    entry.clone()
                };

                let tool_name = session
                    .tool_call_name
                    .get(&tool_delta.id)
                    .cloned()
                    .unwrap_or_else(|| tool_delta.tool_name.clone());

                let message = CachedMessage::tool_result(
                    format!("tool_result:{}", tool_delta.id),
                    tool_name,
                    Some(tool_delta.id.clone()),
                    combined,
                    now_ms,
                    None,
                );
                session.upsert(message);
                Some(session.version)
            } else {
                None
            };

            if let Some(version) = version_opt {
                s.emit_event(ChatRuntimeEvent::CacheUpdated {
                    session_id: session_id.to_string(),
                    version,
                });
            }
            Ok(StreamControl::Continue)
        }
        Some(chat_stream_event::Event::ToolResult(tool_result)) => {
            let outcome = ToolExecutionOutcome {
                tool_call_id: tool_result.id.clone(),
                tool_name: tool_result.tool_name.clone(),
                result: Some(tool_result.result.clone()),
                result_parts: None,
                error: None,
                duration_ms: None,
            };
            turn_state.record_tool_result_message(&outcome);

            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let mut s = state.write().await;
            let version_opt = if let Some(session) = s.sessions.get_mut(session_id) {
                if !tool_result.tool_name.trim().is_empty() {
                    session
                        .tool_call_name
                        .insert(tool_result.id.clone(), tool_result.tool_name.clone());
                }
                session
                    .tool_result_content
                    .insert(tool_result.id.clone(), tool_result.result.clone());

                let tool_name = session
                    .tool_call_name
                    .get(&tool_result.id)
                    .cloned()
                    .unwrap_or_else(|| tool_result.tool_name.clone());

                let message = CachedMessage::tool_result(
                    format!("tool_result:{}", tool_result.id),
                    tool_name,
                    Some(tool_result.id.clone()),
                    tool_result.result.clone(),
                    now_ms,
                    None,
                );
                session.upsert(message);
                Some(session.version)
            } else {
                None
            };

            if let Some(version) = version_opt {
                s.emit_event(ChatRuntimeEvent::CacheUpdated {
                    session_id: session_id.to_string(),
                    version,
                });
            }
            Ok(StreamControl::Continue)
        }
        Some(chat_stream_event::Event::SubSession(sub_session)) => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let mut s = state.write().await;
            let version_opt = if let Some(session) = s.sessions.get_mut(session_id) {
                let draft = session
                    .sub_sessions
                    .entry(sub_session.parent_tool_call_id.clone())
                    .or_insert_with(|| {
                        ProtocolSubSessionDraft::new(
                            sub_session.parent_tool_call_id.clone(),
                            sub_session.sub_session_id.clone(),
                            sub_session.sub_run_id.clone(),
                            sub_session.agent_name.clone(),
                            if sub_session.title.trim().is_empty() {
                                None
                            } else {
                                Some(sub_session.title.clone())
                            },
                            sub_session.depth,
                        )
                    });

                if draft.cached.sub_session_id.is_empty() {
                    draft.cached.sub_session_id = sub_session.sub_session_id.clone();
                }
                if draft.cached.sub_run_id.is_empty() {
                    draft.cached.sub_run_id = sub_session.sub_run_id.clone();
                }
                if draft.cached.agent_name.trim().is_empty() {
                    draft.cached.agent_name = sub_session.agent_name.clone();
                }
                if draft.cached.title.is_none() && !sub_session.title.trim().is_empty() {
                    draft.cached.title = Some(sub_session.title.clone());
                }
                if draft.cached.depth == 0 {
                    draft.cached.depth = sub_session.depth;
                }

                match sub_session.event.as_ref() {
                    Some(chat_proto::chat_sub_session_event::Event::Start(_)) => {
                        draft.cached.status = Some("running".to_string());
                    }
                    Some(chat_proto::chat_sub_session_event::Event::Delta(delta)) => {
                        draft.append_markdown(&delta.content);
                    }
                    Some(chat_proto::chat_sub_session_event::Event::ThinkingStart(_)) => {}
                    Some(chat_proto::chat_sub_session_event::Event::ThinkingEnd(thinking)) => {
                        draft.push_thinking(thinking.content.clone());
                    }
                    Some(chat_proto::chat_sub_session_event::Event::ToolCallStart(tool_call)) => {
                        draft.upsert_tool_call_start(&tool_call.id, &tool_call.tool_name);
                    }
                    Some(chat_proto::chat_sub_session_event::Event::ToolCallDelta(tool_call)) => {
                        draft.append_tool_call_delta(&tool_call.id, &tool_call.arguments_delta);
                    }
                    Some(chat_proto::chat_sub_session_event::Event::ToolDelta(tool_result)) => {
                        draft.append_tool_result_delta(
                            &tool_result.id,
                            &tool_result.tool_name,
                            &tool_result.delta,
                        );
                    }
                    Some(chat_proto::chat_sub_session_event::Event::ToolResult(tool_result)) => {
                        draft.set_tool_result(
                            &tool_result.id,
                            &tool_result.tool_name,
                            tool_result.result.clone(),
                        );
                    }
                    Some(chat_proto::chat_sub_session_event::Event::TurnStart(turn_start)) => {
                        draft.push_turn_start(turn_start.turn);
                    }
                    Some(chat_proto::chat_sub_session_event::Event::Done(done)) => {
                        draft.finish(if done.final_output.trim().is_empty() {
                            None
                        } else {
                            Some(done.final_output.clone())
                        });
                    }
                    Some(chat_proto::chat_sub_session_event::Event::Error(error)) => {
                        draft.fail(error.message.clone());
                    }
                    None => {}
                }

                attach_sub_session_to_parent_tool_call(
                    session,
                    &sub_session.parent_tool_call_id,
                    now_ms,
                );
                Some(session.version)
            } else {
                None
            };

            if let Some(version) = version_opt {
                s.emit_event(ChatRuntimeEvent::CacheUpdated {
                    session_id: session_id.to_string(),
                    version,
                });
            }
            Ok(StreamControl::Continue)
        }
        Some(chat_stream_event::Event::NeedToolExecution(need_tool)) => {
            handle_need_tool_execution_event(
                state,
                session_id,
                assistant_id,
                turn_state,
                &need_tool,
            )
            .await
        }
        Some(chat_stream_event::Event::Done(done)) => {
            let mut s = state.write().await;
            if let Some(session) = s.sessions.get_mut(session_id) {
                turn_state.commit_completed(session, assistant_id);
                session.protocol_state.latest_state = done.state.as_ref().map(prost_struct_to_json);
            }
            Ok(StreamControl::Continue)
        }
        Some(chat_stream_event::Event::Cancelled(_)) => Ok(StreamControl::Cancelled),
        Some(chat_stream_event::Event::Error(err)) => {
            tracing::error!(
                session_id = %session_id,
                assistant_id = %assistant_id,
                error_message = %err.message,
                error_code = ?err.code,
                "Chat stream returned error event"
            );
            Err(if err.message.trim().is_empty() {
                "Agent error".to_string()
            } else {
                err.message.clone()
            })
        }
        Some(chat_stream_event::Event::RunStart(_))
        | Some(chat_stream_event::Event::TurnStart(_))
        | Some(chat_stream_event::Event::Usage(_))
        | Some(chat_stream_event::Event::Custom(_))
        | Some(chat_stream_event::Event::Interrupt(_)) => Ok(StreamControl::Continue),
        None => Ok(StreamControl::Continue),
    }
}

async fn handle_need_tool_execution_event(
    state: &Arc<RwLock<ActorState>>,
    session_id: &str,
    assistant_id: &str,
    turn_state: &mut ProtocolTurnState,
    event: &chat_proto::ChatNeedToolExecutionEvent,
) -> Result<StreamControl, String> {
    let state_json = event
        .state
        .as_ref()
        .map(prost_struct_to_json)
        .ok_or_else(|| "NeedToolExecution missing resume state".to_string())?;
    let normalized_state =
        external_tool_schema::normalize_resume_state_tool_definitions(state_json);

    let completed_results = event
        .completed_results
        .iter()
        .filter_map(proto_tool_outcome_to_runtime)
        .collect::<Vec<_>>();
    tracing::info!(
        session_id = %session_id,
        assistant_id = %assistant_id,
        upstream_completed_results = event.completed_results.len(),
        parsed_completed_results = completed_results.len(),
        parsed_results = ?completed_results.iter().map(summarize_tool_outcome).collect::<Vec<_>>(),
        requested_tool_calls = ?event.tool_calls.iter().map(|tool_call| format!("{}:{}", tool_call.id, tool_call.tool_name)).collect::<Vec<_>>(),
        "Chat runtime received need_tool_execution event"
    );
    for outcome in &completed_results {
        turn_state.record_tool_result_message(outcome);
    }

    let tool_calls = event
        .tool_calls
        .iter()
        .map(|tool_call| ExternalToolCallRequest {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.tool_name.clone(),
            arguments: tool_call
                .arguments
                .as_ref()
                .map(prost_struct_to_json)
                .unwrap_or_else(|| json!({})),
        })
        .collect::<Vec<_>>();

    let external_tool_executor = {
        let s = state.read().await;
        s.external_tool_executor.clone()
    };

    let execution_plan = external_tool_executor.resolve_calls(tool_calls).await;

    for outcome in &execution_plan.resolved_results {
        turn_state.record_tool_result_message(outcome);
    }

    let mut s = state.write().await;
    let version_opt = if let Some(session) = s.sessions.get_mut(session_id) {
        session.protocol_state.latest_state = Some(normalized_state.clone());
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        upsert_tool_outcomes_in_session_cache(session, &completed_results, now_ms);
        upsert_tool_outcomes_in_session_cache(session, &execution_plan.resolved_results, now_ms);
        if execution_plan.pending_calls.is_empty() {
            turn_state.commit_completed(session, assistant_id);
        } else {
            let pending_state = PendingToolExecutionState {
                state: normalized_state.clone(),
                completed_results: completed_results.clone(),
                resolved_results: execution_plan.resolved_results.clone(),
                pending_calls: execution_plan.pending_calls.clone(),
            };
            turn_state.commit_need_tool_execution(session, assistant_id, pending_state);
        }
        Some(session.version)
    } else {
        None
    };

    if let Some(version) = version_opt {
        s.emit_event(ChatRuntimeEvent::CacheUpdated {
            session_id: session_id.to_string(),
            version,
        });
    }

    if execution_plan.trigger_scheduler_sync_needed {
        s.emit_event(ChatRuntimeEvent::TriggerSchedulerSyncRequested);
    }
    if execution_plan.things_changed {
        s.emit_event(ChatRuntimeEvent::ThingsChanged);
    }
    drop(s);

    if let Some(pending) = execution_plan.first_pending_interrupt {
        return Ok(StreamControl::WaitForUser(pending));
    }

    let mut all_results = Vec::new();
    all_results.extend(completed_results);
    all_results.extend(execution_plan.resolved_results);
    let resume_results =
        serialize_tool_outcomes_for_resume(session_id, "auto_resume", &all_results);

    Ok(StreamControl::AutoResume(chat_proto::ChatResumeInput {
        state: Some(json_value_to_prost_struct(normalized_state)),
        results: resume_results,
    }))
}

fn upsert_tool_outcomes_in_session_cache(
    session: &mut SessionState,
    outcomes: &[ToolExecutionOutcome],
    now_ms: i64,
) {
    for outcome in outcomes {
        if !outcome.tool_name.trim().is_empty() {
            session
                .tool_call_name
                .insert(outcome.tool_call_id.clone(), outcome.tool_name.clone());
        }

        let result_text = tool_outcome_display_text(outcome);
        session
            .tool_result_content
            .insert(outcome.tool_call_id.clone(), result_text.clone());

        let message = CachedMessage::tool_result(
            format!("tool_result:{}", outcome.tool_call_id),
            outcome.tool_name.clone(),
            Some(outcome.tool_call_id.clone()),
            result_text,
            now_ms,
            outcome.duration_ms,
        );
        session.upsert(message);
    }
}

fn tool_outcome_display_text(outcome: &ToolExecutionOutcome) -> String {
    if let Some(result) = &outcome.result {
        return result.clone();
    }
    if let Some(error) = &outcome.error {
        return error.clone();
    }
    if let Some(parts) = &outcome.result_parts {
        let mut segments = parts
            .iter()
            .filter_map(|part| image_part_exif_text(part.exif.as_ref()))
            .collect::<Vec<_>>();
        if segments.is_empty() {
            segments.push("Returned image result".to_string());
        }
        return segments.join("\n\n");
    }

    String::new()
}

async fn persist_session(state: &Arc<RwLock<ActorState>>, session_id: &str) {
    let (sdk, messages, protocol_state_json) = {
        let s = state.read().await;
        let session = s.sessions.get(session_id);
        let msgs = session.map(|sess| sess.get_messages()).unwrap_or_default();
        let protocol_state_json = session
            .and_then(|sess| serde_json::to_string(&sess.protocol_state).ok())
            .unwrap_or_else(|| {
                serde_json::to_string(&ChatProtocolSessionState::default()).unwrap_or_default()
            });
        (s.sdk.clone(), msgs, protocol_state_json)
    };

    if let Some(sdk) = sdk {
        let _ = sdk.upsert_chat_runtime_state_json(session_id.to_string(), protocol_state_json);

        // Persist individual messages
        for msg in &messages {
            let _ = sdk.upsert_chat_message_json(
                session_id.to_string(),
                msg.id.clone(),
                msg.timestamp_ms,
                cached_message_storage_json(session_id, msg),
            );
        }

        // Persist session metadata (so it appears in session history)
        // Extract title from first user message, or use a default
        let title = messages.iter().find(|m| m.is_user).map(|m| {
            let content = m.content.trim();
            if content.len() > 50 {
                // Walk back from byte 47 to the nearest char boundary so we never
                // panic on multi-byte UTF-8 characters (e.g. CJK: 3 bytes each).
                let mut cut = 47.min(content.len());
                while !content.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}...", &content[..cut])
            } else {
                content.to_string()
            }
        });

        let message_count = messages.len() as i32;
        let _ = sdk.upsert_chat_session(session_id.to_string(), title, message_count);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Helpers
// ═══════════════════════════════════════════════════════════════════════════════

fn json_value_to_prost_struct(val: JsonValue) -> prost_types::Struct {
    let fields = match val {
        JsonValue::Object(map) => map
            .into_iter()
            .map(|(k, v)| (k, json_value_to_prost_value(v)))
            .collect(),
        _ => {
            let mut m = std::collections::BTreeMap::new();
            m.insert("value".to_string(), json_value_to_prost_value(val));
            m
        }
    };
    prost_types::Struct { fields }
}

fn json_value_to_prost_value(val: JsonValue) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match val {
        JsonValue::Null => Kind::NullValue(0),
        JsonValue::Bool(b) => Kind::BoolValue(b),
        JsonValue::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        JsonValue::String(s) => Kind::StringValue(s),
        JsonValue::Array(arr) => {
            let values = arr.into_iter().map(json_value_to_prost_value).collect();
            Kind::ListValue(prost_types::ListValue { values })
        }
        JsonValue::Object(map) => {
            let fields = map
                .into_iter()
                .map(|(k, v)| (k, json_value_to_prost_value(v)))
                .collect();
            Kind::StructValue(prost_types::Struct { fields })
        }
    };
    prost_types::Value { kind: Some(kind) }
}

fn build_chat_start_metadata(
    config: &ChatRuntimeConfig,
    session_id: &str,
) -> Option<prost_types::Struct> {
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "session_id".to_string(),
        JsonValue::String(session_id.to_string()),
    );
    metadata.insert(
        "traffic_mark".to_string(),
        JsonValue::String(
            match config.backend {
                ChatRuntimeBackend::RemoteServer => "backend",
                ChatRuntimeBackend::LocalWasm(_) => "local_wasm",
            }
            .to_string(),
        ),
    );

    if !config.device_id.trim().is_empty() {
        metadata.insert(
            "device_id".to_string(),
            JsonValue::String(config.device_id.clone()),
        );
    }

    metadata.insert(
        "reporting_consent".to_string(),
        JsonValue::String(config.tracing.reporting_enabled.to_string()),
    );

    if config.tracing.reporting_enabled {
        if let Some(api_key) = config
            .tracing
            .langsmith_api_key
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            metadata.insert(
                "langsmith_api_key".to_string(),
                JsonValue::String(api_key.clone()),
            );
        }
        if let Some(project) = config
            .tracing
            .langsmith_project
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            metadata.insert(
                "langsmith_project".to_string(),
                JsonValue::String(project.clone()),
            );
        }
        if let Some(api_url) = config
            .tracing
            .langsmith_api_url
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            metadata.insert(
                "langsmith_api_url".to_string(),
                JsonValue::String(api_url.clone()),
            );
        }
    }

    normalize_metadata_object_to_string_values(&mut metadata);

    Some(json_value_to_prost_struct(JsonValue::Object(metadata)))
}

fn normalize_metadata_object_to_string_values(metadata: &mut serde_json::Map<String, JsonValue>) {
    metadata.retain(|_, value| match value {
        JsonValue::Null => false,
        JsonValue::String(_) => true,
        JsonValue::Bool(inner) => {
            *value = JsonValue::String(inner.to_string());
            true
        }
        JsonValue::Number(inner) => {
            *value = JsonValue::String(inner.to_string());
            true
        }
        JsonValue::Array(_) | JsonValue::Object(_) => {
            *value = JsonValue::String(value.to_string());
            true
        }
    });
}

fn prost_struct_to_json(s: &prost_types::Struct) -> JsonValue {
    let map: serde_json::Map<String, JsonValue> = s
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), prost_value_to_json(v)))
        .collect();
    JsonValue::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_types::{ChatLocalWasmConfig, ChatRuntimeBackend, ChatTracingConfig};
    use std::sync::Arc;

    #[test]
    fn tool_outcome_proto_round_trip_preserves_image_exif() {
        let expected_exif = json!({
            "fields": [
                {"ifd": "PRIMARY", "tag": "Orientation", "value": 6}
            ]
        });
        let outcome = ToolExecutionOutcome {
            tool_call_id: "call-1".to_string(),
            tool_name: "fetch".to_string(),
            result: None,
            result_parts: Some(vec![crate::chat_types::ToolImagePart {
                media_type: "image/jpeg".to_string(),
                data: vec![1, 2, 3],
                exif: Some(expected_exif.clone()),
            }]),
            error: None,
            duration_ms: None,
        };

        let proto = tool_outcome_to_proto(&outcome).expect("tool outcome proto");
        let round_trip = proto_tool_outcome_to_runtime(&proto).expect("runtime outcome");
        let part = round_trip
            .result_parts
            .and_then(|parts| parts.into_iter().next())
            .expect("image part");

        assert_eq!(part.media_type, "image/jpeg");
        assert_eq!(part.exif, Some(expected_exif));
    }

    #[test]
    fn tool_outcome_proto_round_trip_preserves_text_only_parts() {
        let proto = chat_proto::ChatToolCallOutcome {
            tool_call_id: "agent_doc_read:0".to_string(),
            tool_name: "agent_doc_read".to_string(),
            outcome: Some(chat_proto::chat_tool_call_outcome::Outcome::Parts(
                chat_proto::ChatToolCallResultParts {
                    parts: vec![chat_proto::ChatContentPart {
                        r#type: "text".to_string(),
                        value: Some(chat_proto::chat_content_part::Value::Text(
                            "# manager\ncontent".to_string(),
                        )),
                    }],
                },
            )),
        };

        let round_trip = proto_tool_outcome_to_runtime(&proto).expect("runtime outcome");
        assert_eq!(round_trip.result.as_deref(), Some("# manager\ncontent"));
        assert!(round_trip.result_parts.is_none());

        let encoded = tool_outcome_to_proto(&round_trip).expect("tool outcome proto");
        match encoded.outcome {
            Some(chat_proto::chat_tool_call_outcome::Outcome::Result(result)) => {
                assert_eq!(result, "# manager\ncontent");
            }
            other => panic!("expected plain result, got {:?}", other),
        }
    }

    #[test]
    fn flush_phase_after_need_tool_execution_does_not_duplicate_assistant() {
        let assistant_id = "assistant-tool-phase";
        let mut turn_state = ProtocolTurnState::default();
        let mut session = SessionState::default();

        turn_state.record_tool_call_start("agent_doc_read:0", "agent_doc_read");
        turn_state.record_tool_call_delta("agent_doc_read:0", r#"{"agent_id":"manager"}"#);

        turn_state.commit_need_tool_execution(
            &mut session,
            assistant_id,
            PendingToolExecutionState {
                state: JsonValue::Null,
                completed_results: Vec::new(),
                resolved_results: Vec::new(),
                pending_calls: Vec::new(),
            },
        );

        turn_state.record_tool_result_message(&ToolExecutionOutcome {
            tool_call_id: "agent_doc_read:0".to_string(),
            tool_name: "agent_doc_read".to_string(),
            result: Some("# manager".to_string()),
            result_parts: None,
            error: None,
            duration_ms: None,
        });

        turn_state.flush_phase_into_history(&mut session, assistant_id);

        assert_eq!(session.protocol_state.history.len(), 2);
        assert_eq!(
            session
                .protocol_state
                .history
                .iter()
                .filter(|message| message.id == assistant_id)
                .count(),
            1
        );
        assert_eq!(
            session.protocol_state.history[1].tool_call_id.as_deref(),
            Some("agent_doc_read:0")
        );
    }

    #[test]
    fn build_chat_start_metadata_uses_string_values() {
        let metadata = build_chat_start_metadata(
            &ChatRuntimeConfig {
                device_id: "desktop-machine".to_string(),
                backend: ChatRuntimeBackend::RemoteServer,
                tracing: ChatTracingConfig {
                    reporting_enabled: false,
                    langsmith_api_key: None,
                    langsmith_project: None,
                    langsmith_api_url: None,
                },
                ..Default::default()
            },
            "session-123",
        )
        .expect("metadata");

        let metadata_json = prost_struct_to_json(&metadata);
        assert_eq!(
            metadata_json
                .get("reporting_consent")
                .and_then(|value| value.as_str()),
            Some("false")
        );
        assert_eq!(
            metadata_json
                .get("traffic_mark")
                .and_then(|value| value.as_str()),
            Some("backend")
        );
    }

    #[test]
    fn normalize_metadata_object_to_string_values_converts_non_strings() {
        let mut metadata = serde_json::Map::new();
        metadata.insert("flag".to_string(), JsonValue::Bool(true));
        metadata.insert(
            "count".to_string(),
            JsonValue::Number(serde_json::Number::from(3)),
        );
        metadata.insert("nested".to_string(), json!({ "a": false }));
        metadata.insert("nullish".to_string(), JsonValue::Null);

        normalize_metadata_object_to_string_values(&mut metadata);

        assert_eq!(
            metadata.get("flag").and_then(|value| value.as_str()),
            Some("true")
        );
        assert_eq!(
            metadata.get("count").and_then(|value| value.as_str()),
            Some("3")
        );
        assert_eq!(
            metadata.get("nested").and_then(|value| value.as_str()),
            Some("{\"a\":false}")
        );
        assert!(!metadata.contains_key("nullish"));
    }

    #[test]
    fn local_backend_metadata_still_uses_string_consent() {
        let metadata = build_chat_start_metadata(
            &ChatRuntimeConfig {
                backend: ChatRuntimeBackend::LocalWasm(ChatLocalWasmConfig {
                    source: crate::chat_types::ChatLocalWasmSource::Bytes(Arc::new(vec![])),
                    api_key: String::new(),
                    base_url: None,
                    model: None,
                }),
                tracing: ChatTracingConfig {
                    reporting_enabled: true,
                    langsmith_api_key: None,
                    langsmith_project: None,
                    langsmith_api_url: None,
                },
                ..Default::default()
            },
            "session-456",
        )
        .expect("metadata");

        let metadata_json = prost_struct_to_json(&metadata);
        assert_eq!(
            metadata_json
                .get("reporting_consent")
                .and_then(|value| value.as_str()),
            Some("true")
        );
        assert_eq!(
            metadata_json
                .get("traffic_mark")
                .and_then(|value| value.as_str()),
            Some("local_wasm")
        );
    }

    #[test]
    fn references_are_promoted_into_active_context_json() {
        let references = json!([
            {
                "type": "thing",
                "uuid": "t1",
                "title": "Buy milk",
                "isAutoReference": true,
                "metadata": {"source": "chat"}
            },
            {
                "type": "collection",
                "uuid": "c1",
                "title": "Inbox",
                "mode": "editing"
            }
        ]);

        let active = references_to_active_context_json(Some(&references)).expect("active context");
        assert_eq!(active["viewing"][0]["uuid"], json!("t1"));
        assert_eq!(active["viewing"][0]["is_auto_reference"], json!(true));
        assert_eq!(active["editing"][0]["uuid"], json!("c1"));
    }

    #[test]
    fn merge_active_context_into_user_state_preserves_existing_keys() {
        let merged = merge_active_context_into_user_state(
            Some(json!({"agent_mode": "manager"})),
            Some(json!({"viewing": [{"type": "thing", "uuid": "t1"}]})),
        )
        .expect("merged state");

        assert_eq!(merged["agent_mode"], json!("manager"));
        assert_eq!(merged["active_context"]["viewing"][0]["uuid"], json!("t1"));
    }

    #[test]
    fn uploaded_images_are_exposed_in_user_state_and_prompt() {
        let images = vec![
            UploadedImage {
                remi_uri: "remi://remote/a.png?type=image%2Fpng".to_string(),
            },
            UploadedImage {
                remi_uri: "remi://local/images/b.png?type=image%2Fpng&device=d1".to_string(),
            },
        ];

        let user_state =
            merge_chat_attachments_into_user_state(None, uploaded_images_to_user_state(&images))
                .expect("user state");
        assert_eq!(
            user_state["chat_input_attachments"]["images"][0]["uri"],
            json!("remi://remote/a.png?type=image%2Fpng")
        );

        let prompt = build_chat_attachment_prompt(&images).expect("prompt");
        assert!(prompt.contains("create_tool with type=\"image\""));
        assert!(prompt.contains("remi://remote/a.png?type=image%2Fpng"));
        assert!(prompt.contains("/collection/<collection_uuid>/things/<thing_uuid>"));
    }
}

fn prost_value_to_json(v: &prost_types::Value) -> JsonValue {
    use prost_types::value::Kind;
    match &v.kind {
        None => JsonValue::Null,
        Some(Kind::NullValue(_)) => JsonValue::Null,
        Some(Kind::BoolValue(b)) => JsonValue::Bool(*b),
        Some(Kind::NumberValue(n)) => json!(*n),
        Some(Kind::StringValue(s)) => JsonValue::String(s.clone()),
        Some(Kind::ListValue(list)) => {
            JsonValue::Array(list.values.iter().map(prost_value_to_json).collect())
        }
        Some(Kind::StructValue(s)) => prost_struct_to_json(s),
    }
}
