use std::collections::HashMap;

use remi_agentloop::prelude::Message;

use crate::{CatEvent, MemoryStore};

/// Save new turn messages and user_state to the memory store.
///
/// Strips the first `skip_count` messages (the injected history prefix) from
/// `history` before persisting, so only the new user + assistant messages are
/// appended to short-term storage.
pub(super) async fn persist_turn(
    memory: &MemoryStore,
    thread_id: &str,
    history: Option<Vec<Message>>,
    user_state: Option<serde_json::Value>,
    skip_count: usize,
    tool_elapsed_ms: &HashMap<String, u64>,
) -> Vec<CatEvent> {
    let mut events = Vec::new();
    if let Some(all_msgs) = history {
        let mut new_msgs: Vec<Message> = all_msgs.into_iter().skip(skip_count).collect();
        annotate_tool_elapsed_ms(&mut new_msgs, tool_elapsed_ms);
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
            let mut sink = |event| events.push(CatEvent::ContextCompaction(event));
            if let Err(e) = memory
                .save_turn_with_compaction_events(thread_id, new_msgs, Some(&mut sink))
                .await
            {
                tracing::warn!(thread_id, error = %e, "memory.persist.failed");
            }
        }
    }
    if let Some(us) = user_state {
        if let Err(e) = memory.save_user_state(thread_id, &us).await {
            tracing::warn!(thread_id, error = %e, "memory.user_state.persist.failed");
        }
    }
    events
}

fn annotate_tool_elapsed_ms(messages: &mut [Message], tool_elapsed_ms: &HashMap<String, u64>) {
    for message in messages {
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let Some(elapsed_ms) = tool_elapsed_ms.get(call_id) else {
            continue;
        };
        let metadata = message
            .metadata
            .get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let serde_json::Value::Object(map) = metadata {
            map.insert(
                "tool_elapsed_ms".to_string(),
                serde_json::Value::Number(serde_json::Number::from(*elapsed_ms)),
            );
        }
    }
}

pub(super) async fn persist_intermediate_user_state(
    memory: &MemoryStore,
    thread_id: &str,
    user_state: serde_json::Value,
) -> CatEvent {
    if let Err(e) = memory.save_user_state(thread_id, &user_state).await {
        tracing::warn!(
            thread_id,
            intermediate = true,
            error = %e,
            "memory.user_state.persist.failed"
        );
    }
    CatEvent::StateUpdate(user_state)
}
