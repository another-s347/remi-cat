use std::rc::Rc;

use bot_core::CatEvent;
use futures::StreamExt;

use crate::core::{ChatChannel, ChatRequest, CoreChatEvent, Runtime};
use crate::local_trigger_scheduler::LocalTriggerDispatch;

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
        "executing local trigger dispatch"
    );
    if let Some(tx) = &output_tx {
        let _ = tx.send(format!(
            "触发器「{}」已触发，开始执行。\n",
            dispatch.trigger_name
        ));
    }
    let request = ChatRequest::trigger_text(
        dispatch.thread_id,
        ChatChannel::LocalTrigger,
        dispatch.request,
    )
    .with_sender(dispatch.owner_user_id, dispatch.owner_username)
    .with_message(
        format!("trigger-{}", uuid::Uuid::new_v4()),
        dispatch.chat_type.unwrap_or_else(|| "p2p".to_string()),
    )
    .with_platform(dispatch.platform)
    .enable_sdk_todo();
    let mut stream = std::pin::pin!(Rc::clone(&runtime).chat(request));
    while let Some(event) = stream.next().await {
        match event {
            CoreChatEvent::Bot(CatEvent::Text(delta)) => {
                if let Some(tx) = &output_tx {
                    let _ = tx.send(delta);
                }
            }
            CoreChatEvent::Bot(CatEvent::Error(err)) => {
                return Err(anyhow::anyhow!(err.to_string()))
            }
            CoreChatEvent::Bot(CatEvent::Done) | CoreChatEvent::Done => break,
            _ => {}
        }
    }
    Ok(())
}
