//! Chat runtime types shared across SDK, mobile, and CLI.

use crate::types::ChatSession;
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

// ═══════════════════════════════════════════════════════════════════════════════
// Run State
// ═══════════════════════════════════════════════════════════════════════════════

/// Chat run state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ChatRunState {
    #[default]
    Idle,
    Running,
    WaitingForUser,
    Error,
}

/// Pending interrupt awaiting user action
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingInterrupt {
    pub interrupt_id: String,
    pub interrupt_type: String,
    pub display_data: JsonValue,
}

/// Status information about a chat run
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatRunStatus {
    pub state: ChatRunState,
    pub session_id: Option<String>,
    pub error_message: Option<String>,
    pub pending_interrupt: Option<PendingInterrupt>,
}

/// Versioned export of a persisted chat session for debugging or replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSessionExportBundle {
    pub version: u32,
    pub exported_at: DateTime<Utc>,
    pub session: ChatSession,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<CachedMessage>,
    pub protocol_state: ChatProtocolSessionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_status: Option<ChatRunStatus>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, JsonValue>,
}

impl ChatSessionExportBundle {
    pub const VERSION: u32 = 1;

    pub fn new(
        session: ChatSession,
        messages: Vec<CachedMessage>,
        protocol_state: ChatProtocolSessionState,
    ) -> Self {
        Self {
            version: Self::VERSION,
            exported_at: Utc::now(),
            session,
            messages,
            protocol_state,
            run_status: None,
            metadata: serde_json::Map::new(),
        }
    }

    pub fn with_run_status(mut self, run_status: ChatRunStatus) -> Self {
        self.run_status = Some(run_status);
        self
    }

    pub fn with_metadata(mut self, metadata: serde_json::Map<String, JsonValue>) -> Self {
        self.metadata = metadata;
        self
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Cached Messages
// ═══════════════════════════════════════════════════════════════════════════════

/// A single cached message for UI rendering
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CachedUiElement {
    Markdown {
        text: String,
    },
    Thinking {
        text: String,
    },
    References {
        items: JsonValue,
    },
    Images {
        uris: Vec<String>,
    },
    ToolInvocation {
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        tool_name: String,
        status_text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        response_text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default)]
        failed: bool,
        #[serde(default)]
        running: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        sub_session: Option<CachedSubSession>,
    },
    ToolCall {
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_json: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    ToolResult {
        tool_name: String,
        result: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    SubSession {
        data: CachedSubSession,
    },
    Error {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSubSession {
    pub parent_tool_call_id: String,
    pub sub_session_id: String,
    pub sub_run_id: String,
    pub agent_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<CachedSubSessionItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CachedSubSessionItem {
    Markdown {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        tool_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments_json: Option<String>,
    },
    ToolResult {
        tool_name: String,
        result: String,
    },
    TurnStart {
        turn: u32,
    },
    Error {
        text: String,
    },
}

/// A single cached message for UI rendering
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedMessage {
    pub id: String,
    pub content: String,
    pub is_user: bool,
    pub timestamp_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub references: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_session: Option<CachedSubSession>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ui_elements: Vec<CachedUiElement>,
}

impl CachedMessage {
    pub fn user(id: String, content: String, timestamp_ms: i64) -> Self {
        Self {
            id,
            content,
            is_user: true,
            timestamp_ms,
            thinking: None,
            has_error: None,
            tool_name: None,
            tool_call_id: None,
            tool_result: None,
            duration_ms: None,
            references: None,
            attachments: None,
            sub_session: None,
            ui_elements: Vec::new(),
        }
    }

    pub fn assistant(id: String, timestamp_ms: i64) -> Self {
        Self {
            id,
            content: String::new(),
            is_user: false,
            timestamp_ms,
            thinking: None,
            has_error: None,
            tool_name: None,
            tool_call_id: None,
            tool_result: None,
            duration_ms: None,
            references: None,
            attachments: None,
            sub_session: None,
            ui_elements: Vec::new(),
        }
    }

    pub fn tool_result(
        id: String,
        tool_name: String,
        tool_call_id: Option<String>,
        result: String,
        timestamp_ms: i64,
        duration_ms: Option<u64>,
    ) -> Self {
        Self {
            id,
            content: String::new(),
            is_user: false,
            timestamp_ms,
            thinking: None,
            has_error: None,
            tool_name: Some(tool_name),
            tool_call_id,
            tool_result: Some(result),
            duration_ms,
            references: None,
            attachments: None,
            sub_session: None,
            ui_elements: Vec::new(),
        }
    }

    pub fn refresh_ui_elements(&mut self) {
        self.ui_elements = derive_ui_elements(self);
    }
}

fn derive_ui_elements(message: &CachedMessage) -> Vec<CachedUiElement> {
    let mut elements = Vec::new();
    let image_uris = collect_image_uris(message.attachments.as_ref(), &message.content);
    let markdown_text = strip_image_lines(&message.content);

    if message.is_user {
        append_references(&mut elements, message.references.as_ref());
        if !markdown_text.trim().is_empty() {
            elements.push(CachedUiElement::Markdown {
                text: markdown_text,
            });
        }
        append_images(&mut elements, image_uris);
        return elements;
    }

    if let Some(thinking) = message
        .thinking
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        elements.push(CachedUiElement::Thinking {
            text: thinking.clone(),
        });
    }

    if message.has_error.unwrap_or(false) {
        let text = markdown_text.trim();
        if !text.is_empty() {
            elements.push(CachedUiElement::Error {
                text: text.to_string(),
            });
        }
        append_references(&mut elements, message.references.as_ref());
        append_images(&mut elements, image_uris);
        return elements;
    }

    if let Some(result) = message
        .tool_result
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        elements.push(CachedUiElement::ToolResult {
            tool_name: message
                .tool_name
                .clone()
                .unwrap_or_else(|| "tool".to_string()),
            result: result.clone(),
            tool_call_id: message.tool_call_id.clone(),
            duration_ms: message.duration_ms,
        });
        append_references(&mut elements, message.references.as_ref());
        append_images(&mut elements, image_uris);
        return elements;
    }

    if let Some(tool_name) = message
        .tool_name
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        elements.push(CachedUiElement::ToolCall {
            tool_name: tool_name.clone(),
            arguments_json: parse_tool_call_arguments(&message.content),
            tool_call_id: message.tool_call_id.clone(),
            duration_ms: message.duration_ms,
        });
        if let Some(sub_session) = message.sub_session.clone() {
            elements.push(CachedUiElement::SubSession { data: sub_session });
        }
        append_references(&mut elements, message.references.as_ref());
        append_images(&mut elements, image_uris);
        return elements;
    }

    if !markdown_text.trim().is_empty() {
        elements.push(CachedUiElement::Markdown {
            text: markdown_text,
        });
    }

    append_references(&mut elements, message.references.as_ref());
    append_images(&mut elements, image_uris);
    elements
}

pub fn compact_cached_messages_for_ui(messages: &[CachedMessage]) -> Vec<CachedMessage> {
    let mut compacted = Vec::new();
    let mut consumed_result_indices = std::collections::HashSet::new();
    let mut index = 0usize;

    while index < messages.len() {
        if consumed_result_indices.contains(&index) {
            index += 1;
            continue;
        }

        let message = &messages[index];
        let tool_call = extract_tool_call(message);
        if let Some(tool_call) = tool_call {
            let matched_result_index = find_matching_tool_result_index(
                messages,
                index + 1,
                message,
                &tool_call,
                &consumed_result_indices,
            );
            let matched_result = matched_result_index
                .and_then(|result_index| messages.get(result_index))
                .and_then(extract_tool_result);
            if let Some(result_index) = matched_result_index {
                consumed_result_indices.insert(result_index);
            }

            compacted.push(build_tool_invocation_message(
                Some(message),
                Some(tool_call),
                matched_result_index.and_then(|result_index| messages.get(result_index)),
                matched_result,
            ));
            index += 1;
            continue;
        }

        if let Some(tool_result) = extract_tool_result(message) {
            compacted.push(build_tool_invocation_message(
                None,
                None,
                Some(message),
                Some(tool_result),
            ));
            index += 1;
            continue;
        }

        compacted.push(message.clone());
        index += 1;
    }

    compacted
}

fn find_matching_tool_result_index(
    messages: &[CachedMessage],
    start_index: usize,
    call_message: &CachedMessage,
    tool_call: &ToolCallView,
    consumed_result_indices: &std::collections::HashSet<usize>,
) -> Option<usize> {
    let tool_call_id = tool_call
        .tool_call_id
        .as_deref()
        .or(call_message.tool_call_id.as_deref())
        .or_else(|| parse_tool_call_id_from_message_id(&call_message.id));

    for (index, message) in messages.iter().enumerate().skip(start_index) {
        if consumed_result_indices.contains(&index) {
            continue;
        }

        let Some(tool_result) = extract_tool_result(message) else {
            continue;
        };

        let is_match = if let Some(tool_call_id) = tool_call_id {
            tool_result
                .tool_call_id
                .as_deref()
                .or(message.tool_call_id.as_deref())
                .or_else(|| parse_tool_call_id_from_message_id(&message.id))
                .map(|result_id| result_id == tool_call_id)
                .unwrap_or(false)
        } else {
            tool_messages_match(call_message, message, tool_call, &tool_result)
        };

        if is_match {
            return Some(index);
        }
    }

    None
}

#[derive(Clone)]
struct ToolCallView {
    tool_name: String,
    tool_call_id: Option<String>,
    arguments_json: Option<String>,
    duration_ms: Option<u64>,
    sub_session: Option<CachedSubSession>,
}

#[derive(Clone)]
struct ToolResultView {
    tool_name: String,
    tool_call_id: Option<String>,
    result: String,
    duration_ms: Option<u64>,
}

fn extract_tool_call(message: &CachedMessage) -> Option<ToolCallView> {
    let element = message.ui_elements.iter().find_map(|item| match item {
        CachedUiElement::ToolCall {
            tool_name,
            arguments_json,
            tool_call_id,
            duration_ms,
        } if !tool_name.trim().is_empty() => Some(ToolCallView {
            tool_name: tool_name.clone(),
            tool_call_id: tool_call_id
                .clone()
                .or_else(|| message.tool_call_id.clone()),
            arguments_json: arguments_json.clone(),
            duration_ms: duration_ms.or(message.duration_ms),
            sub_session: message.sub_session.clone(),
        }),
        _ => None,
    })?;

    Some(element)
}

fn extract_tool_result(message: &CachedMessage) -> Option<ToolResultView> {
    message.ui_elements.iter().find_map(|item| match item {
        CachedUiElement::ToolResult {
            tool_name,
            result,
            tool_call_id,
            duration_ms,
        } if !result.trim().is_empty() => Some(ToolResultView {
            tool_name: if tool_name.trim().is_empty() {
                "tool".to_string()
            } else {
                tool_name.clone()
            },
            tool_call_id: tool_call_id
                .clone()
                .or_else(|| message.tool_call_id.clone()),
            result: result.clone(),
            duration_ms: duration_ms.or(message.duration_ms),
        }),
        _ => None,
    })
}

fn tool_messages_match(
    call_message: &CachedMessage,
    result_message: &CachedMessage,
    tool_call: &ToolCallView,
    tool_result: &ToolResultView,
) -> bool {
    let call_id = tool_call
        .tool_call_id
        .as_deref()
        .or(call_message.tool_call_id.as_deref())
        .or_else(|| parse_tool_call_id_from_message_id(&call_message.id));
    let result_id = tool_result
        .tool_call_id
        .as_deref()
        .or(result_message.tool_call_id.as_deref())
        .or_else(|| parse_tool_call_id_from_message_id(&result_message.id));

    if let (Some(call_id), Some(result_id)) = (call_id, result_id) {
        return call_id == result_id;
    }

    !tool_call.tool_name.trim().is_empty() && tool_call.tool_name == tool_result.tool_name
}

fn parse_tool_call_id_from_message_id(message_id: &str) -> Option<&str> {
    message_id
        .strip_prefix("tool_call:")
        .or_else(|| message_id.strip_prefix("tool_result:"))
}

fn build_tool_invocation_message(
    call_message: Option<&CachedMessage>,
    tool_call: Option<ToolCallView>,
    result_message: Option<&CachedMessage>,
    tool_result: Option<ToolResultView>,
) -> CachedMessage {
    let base = call_message
        .or(result_message)
        .cloned()
        .unwrap_or_else(|| CachedMessage::assistant("tool-invocation".to_string(), 0));
    let tool_name = tool_call
        .as_ref()
        .map(|value| value.tool_name.clone())
        .or_else(|| tool_result.as_ref().map(|value| value.tool_name.clone()))
        .unwrap_or_else(|| "tool".to_string());
    let tool_call_id = tool_call
        .as_ref()
        .and_then(|value| value.tool_call_id.clone())
        .or_else(|| {
            tool_result
                .as_ref()
                .and_then(|value| value.tool_call_id.clone())
        })
        .or_else(|| call_message.and_then(|value| value.tool_call_id.clone()))
        .or_else(|| result_message.and_then(|value| value.tool_call_id.clone()))
        .or_else(|| parse_tool_call_id_from_message_id(&base.id).map(ToString::to_string));
    let request_text = tool_call
        .as_ref()
        .and_then(|value| value.arguments_json.clone());
    let response_text = tool_result.as_ref().map(|value| value.result.clone());
    let failed = response_text
        .as_ref()
        .map(|value| is_failed_tool_response(value))
        .unwrap_or(false);
    let running = tool_result.is_none();
    let duration_ms = tool_call
        .as_ref()
        .and_then(|value| value.duration_ms)
        .or_else(|| tool_result.as_ref().and_then(|value| value.duration_ms))
        .or_else(|| derive_tool_duration_ms(call_message, result_message));
    let status_text = build_semantic_tool_status(
        &tool_name,
        request_text.as_deref(),
        response_text.as_deref(),
        failed,
        running,
    );
    let mut ui_elements = Vec::new();
    ui_elements.push(CachedUiElement::ToolInvocation {
        tool_call_id: tool_call_id.clone(),
        tool_name: tool_name.clone(),
        status_text,
        request_text,
        response_text,
        duration_ms,
        failed,
        running,
        sub_session: tool_call
            .as_ref()
            .and_then(|value| value.sub_session.clone()),
    });
    ui_elements.extend(compact_residual_ui_elements(call_message, result_message));

    CachedMessage {
        id: tool_call_id
            .as_ref()
            .map(|value| format!("tool_invocation:{value}"))
            .unwrap_or(base.id),
        content: String::new(),
        is_user: false,
        timestamp_ms: call_message
            .map(|value| value.timestamp_ms)
            .or_else(|| result_message.map(|value| value.timestamp_ms))
            .unwrap_or(base.timestamp_ms),
        thinking: None,
        has_error: Some(failed),
        tool_name: Some(tool_name),
        tool_call_id,
        tool_result: tool_result.as_ref().map(|value| value.result.clone()),
        duration_ms,
        references: None,
        attachments: None,
        sub_session: None,
        ui_elements,
    }
}

fn compact_residual_ui_elements(
    call_message: Option<&CachedMessage>,
    result_message: Option<&CachedMessage>,
) -> Vec<CachedUiElement> {
    let mut elements = Vec::new();

    for message in [call_message, result_message].into_iter().flatten() {
        for element in &message.ui_elements {
            match element {
                CachedUiElement::ToolCall { .. }
                | CachedUiElement::ToolResult { .. }
                | CachedUiElement::SubSession { .. } => {}
                other => elements.push(other.clone()),
            }
        }
    }

    elements
}

fn derive_tool_duration_ms(
    call_message: Option<&CachedMessage>,
    result_message: Option<&CachedMessage>,
) -> Option<u64> {
    let started_at = call_message?.timestamp_ms;
    let finished_at = result_message?.timestamp_ms;
    if finished_at < started_at {
        return None;
    }

    Some((finished_at - started_at) as u64)
}

fn build_semantic_tool_status(
    tool_name: &str,
    request_text: Option<&str>,
    response_text: Option<&str>,
    failed: bool,
    running: bool,
) -> String {
    let normalized_name = tool_name.trim().to_ascii_lowercase();
    let request = parse_structured_payload(request_text);
    let primary_target =
        describe_tool_target(request.as_ref()).or_else(|| describe_response_target(response_text));

    if normalized_name.contains("fetch") {
        return build_status_line(
            primary_target
                .map(|target| format!("Viewing {target}"))
                .unwrap_or_else(|| "Checking".to_string()),
            failed,
            running,
        );
    }
    if normalized_name.contains("cat") || normalized_name.contains("read") {
        return build_status_line(
            primary_target
                .map(|target| format!("Viewing {target}"))
                .unwrap_or_else(|| "Viewing file".to_string()),
            failed,
            running,
        );
    }
    if normalized_name.contains("tree") || normalized_name.contains("list") {
        return build_status_line(
            primary_target
                .map(|target| format!("Checking {target}"))
                .unwrap_or_else(|| "Checking workspace".to_string()),
            failed,
            running,
        );
    }
    if normalized_name.contains("grep") || normalized_name.contains("search") {
        return build_status_line(
            primary_target
                .map(|target| format!("Searching {target}"))
                .unwrap_or_else(|| "Searching".to_string()),
            failed,
            running,
        );
    }
    if normalized_name.contains("apply_patch") || normalized_name.contains("edit") {
        let changed_lines = estimate_changed_lines(request_text);
        let suffix = primary_target
            .map(|target| format!(" in {target}"))
            .unwrap_or_default();
        let base = changed_lines
            .map(|count| format!("Editing {count} lines{suffix}"))
            .unwrap_or_else(|| format!("Editing{suffix}"));
        return build_status_line(base.trim().to_string(), failed, running);
    }
    if normalized_name.contains("create") {
        return build_status_line(
            primary_target
                .map(|target| format!("Creating {target}"))
                .unwrap_or_else(|| "Creating".to_string()),
            failed,
            running,
        );
    }

    let fallback = primary_target
        .map(|target| format!("{} {target}", humanize_tool_name(tool_name)))
        .unwrap_or_else(|| humanize_tool_name(tool_name));
    build_status_line(fallback, failed, running)
}

fn build_status_line(base: String, failed: bool, running: bool) -> String {
    if failed {
        if let Some(rest) = base.strip_prefix("Viewing ") {
            return format!("Failed to view {rest}");
        }
        return format!("{base} failed");
    }
    if running {
        return base;
    }
    if let Some(rest) = base.strip_prefix("Editing ") {
        return format!("Edited {rest}");
    }
    if let Some(rest) = base.strip_prefix("Creating ") {
        return format!("Created {rest}");
    }
    if let Some(rest) = base.strip_prefix("Checking ") {
        return format!("Checked {rest}");
    }
    if let Some(rest) = base.strip_prefix("Searching ") {
        return format!("Searched {rest}");
    }
    if let Some(rest) = base.strip_prefix("Viewing ") {
        return format!("Viewed {rest}");
    }
    format!("Completed {}", base.to_lowercase())
}

fn parse_structured_payload(text: Option<&str>) -> Option<serde_json::Map<String, JsonValue>> {
    let text = text?.trim();
    if text.is_empty() {
        return None;
    }
    match serde_json::from_str::<JsonValue>(text).ok()? {
        JsonValue::Object(map) => Some(map),
        _ => None,
    }
}

fn describe_tool_target(payload: Option<&serde_json::Map<String, JsonValue>>) -> Option<String> {
    let payload = payload?;
    let direct = [
        "path",
        "filePath",
        "uri",
        "url",
        "query",
        "name",
        "tool_name",
    ]
    .into_iter()
    .filter_map(|key| payload.get(key))
    .find_map(|value| value.as_str())
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(shorten_target);
    if direct.is_some() {
        return direct;
    }

    payload
        .get("args")
        .and_then(|value| value.as_object())
        .and_then(|value| describe_tool_target(Some(value)))
}

fn describe_response_target(text: Option<&str>) -> Option<String> {
    let line = text?
        .lines()
        .map(str::trim)
        .find(|value| !value.is_empty())?;
    Some(truncate_preview(line, 60))
}

fn shorten_target(target: &str) -> String {
    if let Ok(url) = url::Url::parse(target) {
        let path = url.path().trim_end_matches('/');
        let combined = if path.is_empty() {
            url.host_str().unwrap_or_default().to_string()
        } else {
            format!("{}{}", url.host_str().unwrap_or_default(), path)
        };
        return truncate_preview(&combined, 60);
    }

    let normalized = target.replace('\\', "/");
    let tail = normalized
        .split('/')
        .filter(|segment| !segment.is_empty())
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("/");
    truncate_preview(&tail, 60)
}

fn humanize_tool_name(tool_name: &str) -> String {
    let normalized = tool_name
        .replace('_', " ")
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = normalized.chars();
    match chars.next() {
        Some(first) => format!(
            "{}{}",
            first.to_ascii_uppercase(),
            chars.collect::<String>()
        ),
        None => "Tool".to_string(),
    }
}

fn estimate_changed_lines(request_text: Option<&str>) -> Option<usize> {
    let text = request_text?;
    let count = text
        .lines()
        .filter(|line| {
            (line.starts_with('+') || line.starts_with('-'))
                && !line.starts_with("+++")
                && !line.starts_with("---")
        })
        .count();
    if count == 0 { None } else { Some(count) }
}

fn is_failed_tool_response(response_text: &str) -> bool {
    let trimmed = response_text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.to_ascii_lowercase().starts_with("error")
        || trimmed.to_ascii_lowercase().starts_with("failed")
    {
        return true;
    }
    serde_json::from_str::<JsonValue>(trimmed)
        .ok()
        .and_then(|value| value.as_object().map(|object| object.contains_key("error")))
        .unwrap_or(false)
}

fn truncate_preview(text: &str, max_length: usize) -> String {
    if text.chars().count() <= max_length {
        return text.to_string();
    }

    let mut truncated = text.chars().take(max_length).collect::<String>();
    while matches!(truncated.chars().last(), Some(' ' | '.' | ',' | ';' | ':')) {
        let _ = truncated.pop();
    }
    format!("{truncated}...")
}

fn append_references(elements: &mut Vec<CachedUiElement>, references: Option<&JsonValue>) {
    let Some(references) = references else {
        return;
    };

    let is_non_empty = match references {
        JsonValue::Array(items) => !items.is_empty(),
        JsonValue::String(text) => !text.trim().is_empty(),
        JsonValue::Null => false,
        _ => true,
    };

    if is_non_empty {
        elements.push(CachedUiElement::References {
            items: references.clone(),
        });
    }
}

fn append_images(elements: &mut Vec<CachedUiElement>, image_uris: Vec<String>) {
    if !image_uris.is_empty() {
        elements.push(CachedUiElement::Images { uris: image_uris });
    }
}

fn collect_image_uris(attachments: Option<&JsonValue>, content: &str) -> Vec<String> {
    let mut uris = Vec::new();

    if let Some(JsonValue::Array(items)) = attachments {
        for item in items {
            let Some(item_obj) = item.as_object() else {
                continue;
            };

            let item_type = item_obj
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if !item_type.is_empty() && item_type != "image" {
                continue;
            }

            let candidate = item_obj
                .get("url")
                .and_then(|value| value.as_str())
                .or_else(|| item_obj.get("uri").and_then(|value| value.as_str()))
                .or_else(|| item_obj.get("image_uri").and_then(|value| value.as_str()))
                .or_else(|| item_obj.get("path").and_then(|value| value.as_str()))
                .map(str::trim)
                .unwrap_or_default();

            if !candidate.is_empty() && !uris.iter().any(|existing| existing == candidate) {
                uris.push(candidate.to_string());
            }
        }
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(uri) = trimmed.strip_prefix("[image] ") {
            let uri = uri.trim();
            if !uri.is_empty() && !uris.iter().any(|existing| existing == uri) {
                uris.push(uri.to_string());
            }
        }
    }

    uris
}

fn strip_image_lines(content: &str) -> String {
    let lines = content
        .lines()
        .filter(|line| !line.trim().starts_with("[image] "))
        .collect::<Vec<_>>();
    lines.join("\n").trim().to_string()
}

fn parse_tool_call_arguments(content: &str) -> Option<String> {
    let start_idx = content.find("```json").or_else(|| content.find("```"))?;
    let fence = if content[start_idx..].starts_with("```json") {
        "```json"
    } else {
        "```"
    };
    let after_start = &content[start_idx + fence.len()..];
    let after_newline = after_start
        .strip_prefix("\r\n")
        .or_else(|| after_start.strip_prefix('\n'))
        .unwrap_or(after_start);
    let end_idx = after_newline.find("```")?;
    let args = after_newline[..end_idx].trim();
    if args.is_empty() {
        None
    } else {
        Some(args.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{CachedMessage, CachedUiElement, compact_cached_messages_for_ui};

    #[test]
    fn compact_cached_messages_merges_tool_call_and_result() {
        let mut tool_call = CachedMessage::assistant("tool_call:call-1".to_string(), 1_000);
        tool_call.content =
            "Tool call: `fetch`\n\n```json\n{\"url\":\"https://example.com/docs\"}\n```"
                .to_string();
        tool_call.tool_name = Some("fetch".to_string());
        tool_call.tool_call_id = Some("call-1".to_string());
        tool_call.refresh_ui_elements();

        let mut tool_result = CachedMessage::tool_result(
            "tool_result:call-1".to_string(),
            "fetch".to_string(),
            Some("call-1".to_string()),
            "{\"title\":\"Docs\"}".to_string(),
            1_450,
            None,
        );
        tool_result.refresh_ui_elements();

        let compacted = compact_cached_messages_for_ui(&[tool_call, tool_result]);
        assert_eq!(compacted.len(), 1);

        match &compacted[0].ui_elements[0] {
            CachedUiElement::ToolInvocation {
                tool_call_id,
                tool_name,
                status_text,
                request_text,
                response_text,
                duration_ms,
                failed,
                running,
                ..
            } => {
                assert_eq!(tool_call_id.as_deref(), Some("call-1"));
                assert_eq!(tool_name, "fetch");
                assert_eq!(status_text, "Viewed example.com/docs");
                assert_eq!(
                    request_text.as_deref(),
                    Some("{\"url\":\"https://example.com/docs\"}")
                );
                assert_eq!(response_text.as_deref(), Some("{\"title\":\"Docs\"}"));
                assert_eq!(*duration_ms, Some(450));
                assert!(!failed);
                assert!(!running);
            }
            other => panic!("expected tool invocation element, got {other:?}"),
        }
    }

    #[test]
    fn compact_cached_messages_keeps_running_tool_call_open() {
        let mut tool_call = CachedMessage::assistant("tool_call:call-2".to_string(), 2_000);
        tool_call.content =
            "Tool call: `grep_search`\n\n```json\n{\"query\":\"tool invocation\"}\n```".to_string();
        tool_call.tool_name = Some("grep_search".to_string());
        tool_call.tool_call_id = Some("call-2".to_string());
        tool_call.refresh_ui_elements();

        let compacted = compact_cached_messages_for_ui(&[tool_call]);
        assert_eq!(compacted.len(), 1);

        match &compacted[0].ui_elements[0] {
            CachedUiElement::ToolInvocation {
                tool_call_id,
                tool_name,
                status_text,
                response_text,
                running,
                ..
            } => {
                assert_eq!(tool_call_id.as_deref(), Some("call-2"));
                assert_eq!(tool_name, "grep_search");
                assert_eq!(status_text, "Searching tool invocation");
                assert!(response_text.is_none());
                assert!(*running);
            }
            other => panic!("expected tool invocation element, got {other:?}"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Protocol Session State
// ═══════════════════════════════════════════════════════════════════════════════

/// Device-owned protocol session state used to build start/resume requests.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatProtocolSessionState {
    #[serde(default)]
    pub history: Vec<ProtocolHistoryMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_state: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_tool_execution: Option<PendingToolExecutionState>,
}

/// A single remi-agentloop history message persisted by the runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolHistoryMessage {
    pub id: String,
    pub role: String,
    pub content: JsonValue,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ProtocolToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolToolCall {
    pub id: String,
    pub tool_name: String,
    pub arguments: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionOutcome {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Multimodal result parts (e.g. raw image bytes). Mutually exclusive with `result`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_parts: Option<Vec<ToolImagePart>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// A raw image returned by a tool handler (before proto serialisation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolImagePart {
    pub media_type: String,
    pub data: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exif: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: JsonValue,
    pub display_data: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingToolExecutionState {
    pub state: JsonValue,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_results: Vec<ToolExecutionOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved_results: Vec<ToolExecutionOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_calls: Vec<PendingToolCall>,
}

/// Rich result that a handler can return — either plain JSON or structured multimodal data.
#[derive(Debug, Clone)]
pub enum RichHandlerResult {
    /// Plain JSON resume value (the original behaviour).
    Json(JsonValue),
    /// Raw image bytes to be forwarded to the LLM as an image part.
    Image(ToolImagePart),
}

// ═══════════════════════════════════════════════════════════════════════════════
// Runtime Events (for subscribers)
// ═══════════════════════════════════════════════════════════════════════════════

/// Events emitted by the chat runtime
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatRuntimeEvent {
    /// Status changed
    StatusChanged {
        state: ChatRunState,
        session_id: Option<String>,
        error: Option<String>,
    },

    /// Message cache updated
    CacheUpdated { session_id: String, version: u64 },

    /// Pending interrupt for UI
    InterruptPending {
        session_id: String,
        interrupt_id: String,
        interrupt_type: String,
        display_data: JsonValue,
    },

    /// Full pending external-tool execution state for UI flows that need all calls.
    NeedToolExecution {
        session_id: String,
        pending_calls: Vec<PendingToolCall>,
        pending_tool_execution: PendingToolExecutionState,
    },

    /// Things were modified (for UI refresh)
    ThingsChanged,

    /// A trigger was installed/updated via an interrupt; platform schedulers should resync.
    TriggerSchedulerSyncRequested,
}

// ═══════════════════════════════════════════════════════════════════════════════
// Configuration
// ═══════════════════════════════════════════════════════════════════════════════

/// Configuration for chat runtime
#[derive(Debug, Clone)]
pub struct ChatRuntimeConfig {
    /// Device ID for things operations
    pub device_id: String,
    /// Request timeout in seconds
    pub request_timeout_secs: u64,
    /// Maximum auto-resume attempts (prevents infinite loops)
    pub max_auto_resumes: usize,
    /// Execution backend for chat requests.
    pub backend: ChatRuntimeBackend,
    /// Consent-gated tracing configuration shared by remote and local-wasm backends.
    pub tracing: ChatTracingConfig,
}

#[derive(Debug, Clone, Default)]
pub struct ChatTracingConfig {
    /// True only when the user has explicitly allowed reporting/tracing.
    pub reporting_enabled: bool,
    /// Optional LangSmith API key. For remote mode this may be omitted and resolved server-side.
    pub langsmith_api_key: Option<String>,
    /// Optional LangSmith project/session name.
    pub langsmith_project: Option<String>,
    /// Optional LangSmith API URL override.
    pub langsmith_api_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub enum ChatRuntimeBackend {
    #[default]
    /// Remote server-hosted chat stream.
    RemoteServer,
    /// Device-local execution using the embedded or configured WASM/native agent runtime.
    LocalWasm(ChatLocalWasmConfig),
}

#[derive(Debug, Clone)]
pub struct ChatLocalWasmConfig {
    pub source: ChatLocalWasmSource,
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ChatLocalWasmSource {
    File(PathBuf),
    /// In-memory bytes for either a raw `.wasm` or a precompiled `.cwasm`.
    ///
    /// The runtime auto-detects the format from the magic header:
    /// - `\0asm` → JIT-compiled (requires `local-wasm-compiler` feature).
    /// - Anything else → treated as a precompiled artifact (`Component::deserialize`);
    ///   no Cranelift JIT needed, safe on Android.
    Bytes(Arc<Vec<u8>>),
}

impl Default for ChatRuntimeConfig {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            request_timeout_secs: 120,
            max_auto_resumes: 16,
            backend: ChatRuntimeBackend::RemoteServer,
            tracing: ChatTracingConfig::default(),
        }
    }
}
