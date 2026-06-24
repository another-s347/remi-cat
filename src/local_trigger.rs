use std::rc::Rc;

use bot_core::{CatEvent, Content, StreamOptions};
use futures::StreamExt;

use crate::core::Runtime;
use crate::local_trigger_scheduler::LocalTriggerDispatch;
use crate::{SESSION_AGENT_ID_METADATA_KEY, SESSION_MODEL_PROFILE_METADATA_KEY};

pub(crate) async fn run_dispatcher(
    runtime: Rc<Runtime>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<LocalTriggerDispatch>,
    output_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) {
    while let Some(dispatch) = rx.recv().await {
        let runtime = Rc::clone(&runtime);
        let output_tx = output_tx.clone();
        tokio::task::spawn_local(async move {
            if let Err(err) = run_dispatch(runtime, dispatch, output_tx).await {
                tracing::warn!(error = %err, "local trigger dispatch failed");
            }
        });
    }
}

pub(crate) async fn run_dispatch(
    runtime: Rc<Runtime>,
    dispatch: LocalTriggerDispatch,
    output_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> anyhow::Result<()> {
    tracing::info!(
        trigger_uuid = %dispatch.trigger_uuid,
        trigger_name = %dispatch.trigger_name,
        thread_id = %dispatch.thread_id,
        "running local trigger dispatch"
    );
    if let Some(tx) = &output_tx {
        let _ = tx.send(format!(
            "触发器「{}」已触发，开始执行。\n",
            dispatch.trigger_name
        ));
    }
    let (model_profile_id, agent_id) = {
        let sessions = runtime.sessions.lock().await;
        (
            sessions.metadata_string(&dispatch.thread_id, SESSION_MODEL_PROFILE_METADATA_KEY),
            sessions.metadata_string(&dispatch.thread_id, SESSION_AGENT_ID_METADATA_KEY),
        )
    };
    let opts = StreamOptions {
        model_profile_id,
        agent_id,
        sender_user_id: Some(dispatch.owner_user_id),
        sender_username: dispatch.owner_username,
        message_id: Some(format!("trigger-{}", uuid::Uuid::new_v4())),
        chat_type: dispatch.chat_type.or_else(|| Some("p2p".to_string())),
        platform: dispatch.platform,
        todo_create_via_sdk: true,
        trigger_tools_enabled: false,
        trigger_run: true,
        ..StreamOptions::default()
    };
    let mut stream = std::pin::pin!(runtime.bot.stream_with_options(
        &dispatch.thread_id,
        Content::text(dispatch.request),
        opts,
    ));
    while let Some(event) = stream.next().await {
        match event {
            CatEvent::Text(delta) => {
                if let Some(tx) = &output_tx {
                    let _ = tx.send(delta);
                }
            }
            CatEvent::Error(err) => return Err(anyhow::anyhow!(err.to_string())),
            CatEvent::Done => break,
            _ => {}
        }
    }
    Ok(())
}
