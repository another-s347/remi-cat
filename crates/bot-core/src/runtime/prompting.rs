use remi_agentloop::prelude::{Content, ContentPart, LoopInput, Message, Role};

use crate::{
    estimate_model_input_tokens, skill, todo, ModelInputSegment, ModelInputSegmentCategory,
    ModelInputSnapshot, ModelInputTotals, SkillDocument,
};

pub(crate) fn truncate_user_name(name: Option<&str>, max_chars: usize) -> Option<String> {
    let trimmed = name?.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(max_chars).collect())
}

pub(crate) fn insert_single_chat_sender_system_prompt(
    history: &mut Vec<Message>,
    insertion_index: usize,
    prompt: Option<String>,
) {
    let Some(prompt) = prompt else {
        return;
    };
    history.insert(insertion_index.min(history.len()), Message::system(prompt));
}

pub(crate) fn insert_skill_injection_prompts(
    history: &mut Vec<Message>,
    insertion_index: usize,
    skills: &[SkillDocument],
) {
    if skills.is_empty() {
        return;
    }
    let mut offset = 0;
    for skill in skills {
        let resource_hint = match (&skill.skill_file_path, &skill.resource_root_path) {
            (Some(skill_file), Some(resource_root)) => format!(
                "Use fs_read with skill_file_path={skill_file}; resource_root_path={resource_root} for supporting files."
            ),
            _ => "Supporting files for this skill are not readable through fs_read because the skill is outside the workspace root.".to_string(),
        };
        let prompt = format!(
            "Skill `{}` loaded for this turn from {}. {}\n\n{}",
            skill.name,
            skill.source,
            resource_hint,
            skill.content.trim_end()
        );
        history.insert(
            (insertion_index + offset).min(history.len()),
            Message::system(prompt),
        );
        offset += 1;
    }
}

pub(crate) fn apply_skill_injections(user_state: &mut serde_json::Value, skills: &[SkillDocument]) {
    if skills.is_empty() {
        return;
    }
    if !user_state.is_object() {
        *user_state = serde_json::json!({});
    }
    let Some(map) = user_state.as_object_mut() else {
        return;
    };
    let entry = map
        .entry(skill::tools::READ_SKILLS_STATE_KEY.to_string())
        .or_insert_with(|| serde_json::json!([]));
    if !entry.is_array() {
        *entry = serde_json::json!([]);
    }
    let Some(items) = entry.as_array_mut() else {
        return;
    };
    for skill in skills {
        if !items
            .iter()
            .any(|item| item.as_str().is_some_and(|value| value == skill.name))
        {
            items.push(serde_json::Value::String(skill.name.clone()));
        }
    }
}

pub(crate) fn model_input_snapshot_from_loop_input(
    input: &LoopInput,
    thread_id: &str,
    run_id: Option<&str>,
    model_profile_id: &str,
    model: &str,
) -> Option<ModelInputSnapshot> {
    let LoopInput::Start {
        content,
        history,
        metadata,
        message_metadata,
        user_state,
        ..
    } = input
    else {
        return None;
    };

    let mut segments = Vec::new();
    for (index, message) in history.iter().enumerate() {
        append_message_model_input_segments(&mut segments, index, message);
    }
    append_segment(
        &mut segments,
        ModelInputSegmentCategory::CurrentUser,
        Some("user".to_string()),
        "Current user input".to_string(),
        content_to_model_input_text(content),
    );
    if let Some(metadata) = metadata {
        append_json_segment(
            &mut segments,
            ModelInputSegmentCategory::Metadata,
            "Request metadata",
            metadata,
        );
    }
    if let Some(message_metadata) = message_metadata {
        append_json_segment(
            &mut segments,
            ModelInputSegmentCategory::Metadata,
            "Message metadata",
            message_metadata,
        );
    }
    if let Some(user_state) = user_state {
        append_json_segment(
            &mut segments,
            ModelInputSegmentCategory::UserState,
            "User state",
            user_state,
        );
    }

    let estimated_tokens = segments.iter().fold(0_u32, |sum, segment| {
        sum.saturating_add(segment.token_estimate)
    });
    Some(ModelInputSnapshot {
        run_id: run_id
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        thread_id: thread_id.to_string(),
        model_profile_id: model_profile_id.to_string(),
        model: model.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        segments,
        totals: ModelInputTotals {
            estimated_tokens,
            ..ModelInputTotals::default()
        },
    })
}

fn append_message_model_input_segments(
    segments: &mut Vec<ModelInputSegment>,
    index: usize,
    message: &Message,
) {
    if let Some(tool_calls) = message.tool_calls.as_ref() {
        for (tool_index, call) in tool_calls.iter().enumerate() {
            append_segment(
                segments,
                ModelInputSegmentCategory::ToolInput,
                Some(role_name(&message.role)),
                format!("Tool input {}.{}", index + 1, tool_index + 1),
                serde_json::to_string_pretty(call).unwrap_or_else(|_| format!("{call:?}")),
            );
        }
    }

    let category = if message.tool_call_id.is_some() {
        ModelInputSegmentCategory::ToolOutput
    } else if message.role == Role::System {
        let text = content_to_model_input_text(&message.content);
        if text.starts_with("Skill `") && text.contains(" loaded for this turn from ") {
            ModelInputSegmentCategory::SkillInjection
        } else {
            ModelInputSegmentCategory::SystemPrompt
        }
    } else {
        ModelInputSegmentCategory::History
    };

    append_segment(
        segments,
        category,
        Some(role_name(&message.role)),
        message_segment_title(index, message),
        content_to_model_input_text(&message.content),
    );
    if let Some(reasoning) = &message.reasoning_content {
        append_segment(
            segments,
            ModelInputSegmentCategory::History,
            Some(role_name(&message.role)),
            format!("Reasoning {}", index + 1),
            reasoning.clone(),
        );
    }
    if let Some(metadata) = &message.metadata {
        append_json_segment(
            segments,
            ModelInputSegmentCategory::Metadata,
            &format!("Message metadata {}", index + 1),
            metadata,
        );
    }
}

fn message_segment_title(index: usize, message: &Message) -> String {
    if let Some(tool_call_id) = &message.tool_call_id {
        return format!("Tool output {} ({tool_call_id})", index + 1);
    }
    format!("{} {}", role_name(&message.role), index + 1)
}

fn role_name(role: &Role) -> String {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
    .to_string()
}

fn append_json_segment(
    segments: &mut Vec<ModelInputSegment>,
    category: ModelInputSegmentCategory,
    title: &str,
    value: &serde_json::Value,
) {
    append_segment(
        segments,
        category,
        None,
        title.to_string(),
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    );
}

fn append_segment(
    segments: &mut Vec<ModelInputSegment>,
    category: ModelInputSegmentCategory,
    role: Option<String>,
    title: String,
    content: String,
) {
    let id = format!("seg_{}", segments.len() + 1);
    let token_estimate = estimate_model_input_tokens(&content);
    segments.push(ModelInputSegment {
        id,
        category,
        role,
        title,
        content,
        token_estimate,
    });
}

fn content_to_model_input_text(content: &Content) -> String {
    match content {
        Content::Text(text) => text.clone(),
        Content::Parts(parts) => parts
            .iter()
            .map(content_part_to_model_input_text)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn content_part_to_model_input_text(part: &ContentPart) -> String {
    match part {
        ContentPart::Text { text } => text.clone(),
        ContentPart::ImageUrl { image_url } => {
            format!("[image_url] {}", image_url.url)
        }
        ContentPart::ImageBase64 { media_type, data } => {
            format!("[image_base64] {media_type}, {} bytes", data.len())
        }
        ContentPart::Audio { .. } => "[audio]".to_string(),
        ContentPart::File { .. } => {
            serde_json::to_string(part).unwrap_or_else(|_| "[file]".to_string())
        }
    }
}

pub(crate) fn append_thread_todo_system_prompt(
    history: &mut Vec<Message>,
    user_state: &serde_json::Value,
) {
    if let Some(prompt) = todo::latest_unfinished_batch_system_prompt(user_state) {
        history.push(Message::system(prompt));
    }
}

pub(crate) fn route_thread_todo_prompt(
    history: &mut Vec<Message>,
    user_state: &serde_json::Value,
    supervisor_active: bool,
) -> Option<String> {
    if supervisor_active {
        todo::latest_unfinished_batch_system_prompt(user_state)
    } else {
        append_thread_todo_system_prompt(history, user_state);
        None
    }
}

pub(crate) fn single_chat_sender_system_prompt(
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

pub(crate) fn prepend_group_sender_username(
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

pub(crate) fn is_direct_chat(chat_type: Option<&str>) -> bool {
    chat_type
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("p2p"))
}
