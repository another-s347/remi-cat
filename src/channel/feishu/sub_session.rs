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
        let messages =
            crate::sub_session::history_messages_for_run(&runtime.data_dir, session_id, event)
                .await;
        if let Err(err) = runtime
            .bot
            .append_thread_messages(session_id, messages)
            .await
        {
            warn!("failed to append sub-session history: {err:#}");
        }
    }

    if let Some(session_id) = sub_session_id.as_deref() {
        if let Err(err) =
            crate::sub_session::append_event_log(&runtime.data_dir, session_id, event).await
        {
            warn!("failed to append sub-session event log: {err:#}");
        }
    }

    if !matches!(event.event.as_ref(), ProtocolEvent::RunStart { .. }) {
        return;
    }
    if platform != FEISHU_CHANNEL && !platform.starts_with("feishu:") {
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
