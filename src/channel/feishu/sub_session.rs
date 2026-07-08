use bot_core::im_tools::SubSessionBindingUpsertRequest;
use im_feishu::FeishuMessage;
use remi_agentloop::prelude::ProtocolEvent;
use remi_agentloop::types::SubSessionEvent;
use tracing::warn;

use crate::app::FEISHU_CHANNEL;
use crate::core::Runtime;
use crate::session::{ChannelBinding, SubSessionKind};

pub(super) async fn record_sub_session_event(
    runtime: &Runtime,
    parent_session_id: &str,
    platform: &str,
    msg: &FeishuMessage,
    event: &SubSessionEvent,
) {
    let kind = if event.agent_name == "acp" {
        SubSessionKind::Acp
    } else {
        SubSessionKind::Agent
    };
    let status = match event.event.as_ref() {
        ProtocolEvent::Done => "done",
        ProtocolEvent::Custom { event_type, .. } if event_type == "sub_session_done" => "done",
        ProtocolEvent::Error { .. } | ProtocolEvent::Cancelled => "error",
        _ => "running",
    };
    if let Err(err) = runtime.sessions.lock().await.upsert_sub_session(
        parent_session_id,
        &event.sub_thread_id.0,
        kind.clone(),
        &event.agent_name,
        event.title.clone(),
        status,
    ) {
        warn!("failed to record sub-session: {err:#}");
    }

    let sub_session_id =
        ensure_sub_session_channel_session(runtime, parent_session_id, platform, event).await;
    if let Some(session_id) = sub_session_id.as_deref() {
        let messages = sub_session_history_messages(event);
        if let Err(err) = runtime
            .bot
            .append_thread_messages(session_id, messages)
            .await
        {
            warn!("failed to append sub-session history: {err:#}");
        }
    }

    if !matches!(event.event.as_ref(), ProtocolEvent::RunStart { .. }) {
        if platform == FEISHU_CHANNEL {
            forward_sub_session_event_to_bound_channel(runtime, parent_session_id, event).await;
        }
        return;
    }
    if platform != FEISHU_CHANNEL {
        return;
    }
    let already_bound = runtime
        .sessions
        .lock()
        .await
        .sub_session_channel_binding(parent_session_id, &event.sub_thread_id.0)
        .is_some();
    if already_bound {
        return;
    }

    let binding = runtime
        .im_bridge
        .sub_session_binding_upsert(SubSessionBindingUpsertRequest {
            parent_session_id: parent_session_id.to_string(),
            sub_session_id: event.sub_thread_id.0.clone(),
            kind: if kind == SubSessionKind::Acp {
                "acp".to_string()
            } else {
                "agent".to_string()
            },
            target: event.agent_name.clone(),
            title: event.title.clone(),
            platform: platform.to_string(),
            parent_channel_id: msg.chat_id.clone(),
            parent_thread_id: None,
            actor_user_id: Some(msg.sender_user_id.clone()),
        })
        .await;
    match binding {
        Ok(Some(binding)) => {
            if let Err(err) = runtime.sessions.lock().await.bind_sub_session_channel(
                parent_session_id,
                &event.sub_thread_id.0,
                ChannelBinding {
                    platform: binding.platform.clone(),
                    channel_id: binding.channel_id.clone(),
                },
            ) {
                warn!("failed to persist sub-session IM binding: {err:#}");
            }
            if let Some(session_id) = sub_session_id.as_deref() {
                if let Err(err) = runtime.sessions.lock().await.set_channel_binding(
                    session_id,
                    &binding.platform,
                    &binding.channel_id,
                ) {
                    warn!("failed to bind sub-session session channel: {err:#}");
                }
            }
        }
        Ok(None) => {}
        Err(err) => warn!("failed to create sub-session IM binding: {err:#}"),
    }
}

async fn ensure_sub_session_channel_session(
    runtime: &Runtime,
    parent_session_id: &str,
    platform: &str,
    event: &SubSessionEvent,
) -> Option<String> {
    let title = event.title.clone().or_else(|| {
        Some(format!(
            "{} {}",
            event.agent_name,
            event.sub_thread_id.0.trim()
        ))
    });
    let existing_binding = runtime
        .sessions
        .lock()
        .await
        .sub_session_channel_binding(parent_session_id, &event.sub_thread_id.0);
    let (session_platform, session_channel_id) = existing_binding
        .as_ref()
        .map(|binding| (binding.platform.as_str(), binding.channel_id.as_str()))
        .unwrap_or((platform, event.sub_thread_id.0.as_str()));
    let session = runtime.sessions.lock().await.create_channel(
        session_platform,
        session_channel_id,
        &runtime.root_agent_id,
        title,
    );
    let session = match session {
        Ok(session) => session,
        Err(err) => {
            warn!("failed to create sub-session channel session: {err:#}");
            return None;
        }
    };
    let mut sessions = runtime.sessions.lock().await;
    let _ = sessions.set_metadata_string(
        &session.id,
        "sub_session_parent_session_id",
        parent_session_id,
    );
    let _ =
        sessions.set_metadata_string(&session.id, "sub_session_thread_id", &event.sub_thread_id.0);
    let _ = sessions.set_metadata_string(&session.id, "sub_session_agent", &event.agent_name);
    drop(sessions);
    if event.agent_name == "acp" {
        if let Some(acp_session_id) = runtime
            .bot
            .acp_session_id_for_sub_session(&event.sub_thread_id.0)
            .await
        {
            let _ = runtime.sessions.lock().await.set_metadata_string(
                &session.id,
                "sub_session_acp_session_id",
                &acp_session_id,
            );
        }
    }
    Some(session.id)
}

async fn forward_sub_session_event_to_bound_channel(
    runtime: &Runtime,
    parent_session_id: &str,
    event: &SubSessionEvent,
) {
    let Some(text) = sub_session_event_text(event) else {
        return;
    };
    let Some(binding) = runtime
        .sessions
        .lock()
        .await
        .sub_session_channel_binding(parent_session_id, &event.sub_thread_id.0)
    else {
        return;
    };
    let done = matches!(
        event.event.as_ref(),
        ProtocolEvent::Done | ProtocolEvent::Error { .. } | ProtocolEvent::Cancelled
    ) || matches!(
        event.event.as_ref(),
        ProtocolEvent::Custom { event_type, .. } if event_type == "sub_session_done"
    );
    if let Err(err) = runtime
        .im_bridge
        .sub_session_send_text(&binding.platform, &binding.channel_id, &text, done)
        .await
    {
        warn!("failed to send sub-session event to IM channel: {err:#}");
    }
}

fn sub_session_history_messages(event: &SubSessionEvent) -> Vec<remi_agentloop::prelude::Message> {
    match event.event.as_ref() {
        ProtocolEvent::RunStart { .. } => {
            let input = event
                .title
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "Sub-session started: {} / {}",
                        event.agent_name, event.sub_thread_id.0
                    )
                });
            vec![remi_agentloop::prelude::Message::user(input)]
        }
        ProtocolEvent::Delta { content, .. } => {
            if content.trim().is_empty() {
                Vec::new()
            } else {
                vec![remi_agentloop::prelude::Message::assistant(content.clone())]
            }
        }
        ProtocolEvent::Custom { event_type, extra } if event_type == "sub_session_done" => extra
            .get("final_output")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                vec![remi_agentloop::prelude::Message::assistant(
                    value.to_string(),
                )]
            })
            .unwrap_or_default(),
        ProtocolEvent::Error { message, .. } => {
            vec![remi_agentloop::prelude::Message::assistant(format!(
                "Error: {message}"
            ))]
        }
        ProtocolEvent::Cancelled => {
            vec![remi_agentloop::prelude::Message::assistant(
                "Error: cancelled",
            )]
        }
        _ => Vec::new(),
    }
}

fn sub_session_event_text(event: &SubSessionEvent) -> Option<String> {
    let text = match event.event.as_ref() {
        ProtocolEvent::RunStart { .. } => return None,
        ProtocolEvent::Delta { content, .. } => content.clone(),
        ProtocolEvent::ThinkingStart => "Thinking...".to_string(),
        ProtocolEvent::ThinkingEnd { content } => {
            if content.trim().is_empty() {
                "Thinking complete.".to_string()
            } else {
                format!("Thinking:\n{content}")
            }
        }
        ProtocolEvent::TurnStart { turn } => format!("Turn {turn}"),
        ProtocolEvent::ToolCallStart { id, name } => {
            format!("Tool `{name}` started ({id}).")
        }
        ProtocolEvent::ToolCallDelta {
            id,
            arguments_delta,
        } => {
            format!("Tool arguments `{id}`:\n{arguments_delta}")
        }
        ProtocolEvent::ToolDelta { id, name, delta } => {
            format!("Tool `{name}` output ({id}):\n{delta}")
        }
        ProtocolEvent::ToolResult { id, name, result } => {
            format!("Tool `{name}` result ({id}):\n{result}")
        }
        ProtocolEvent::Done => "Done.".to_string(),
        ProtocolEvent::Custom { event_type, extra } if event_type == "sub_session_done" => extra
            .get("final_output")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "Done.".to_string()),
        ProtocolEvent::Error { message, .. } => format!("Error: {message}"),
        ProtocolEvent::Cancelled => "Error: cancelled".to_string(),
        other => format!("{other:?}"),
    };
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}
