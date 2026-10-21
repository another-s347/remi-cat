//! Real user ingress and native mention delivery in an authorized acceptance chat.
use super::*;
use bot_core::CatBotBuilder;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires authorized Feishu user messages, bot API credentials and a real model"]
async fn live_user_request_sends_native_mention() -> anyhow::Result<()> {
    tokio::task::LocalSet::new()
        .run_until(run_live_mention())
        .await
}

async fn run_live_mention() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::var("REMI_MENTION_LIVE_DIR")?);
    let marker = std::env::var("REMI_MENTION_LIVE_MARKER")?;
    let chat = std::env::var("REMI_MENTION_LIVE_CHAT")?;
    let gateway = FeishuGateway::new(
        std::env::var("FEISHU_APP_ID")?,
        std::env::var("FEISHU_APP_SECRET")?,
    );
    let bridge = Arc::new(LocalImFileBridge::new(Some(gateway.clone())));
    let data_dir = root.join("data");
    std::fs::create_dir_all(&data_dir)?;
    let runtime = Rc::new(Runtime {
        bot: Rc::new(
            CatBotBuilder::from_env()?
                .im_bridge(bridge.clone())
                .build()?,
        ),
        secret_store: Arc::new(Mutex::new(crate::secret_store::SecretStore::dotenv(
            root.join("unused.env"),
        ))),
        user_store: Arc::new(UserStore::load(&data_dir.join("users.json"))?),
        sessions: Arc::new(Mutex::new(crate::session::SessionRuntime::load(&data_dir)?)),
        im_bridge: bridge,
        root_agent_id: "default".into(),
        data_dir,
    });
    let mut incoming = gateway.start().await?;
    let (anchor, thread) = gateway
        .send_topic_text(
            &chat,
            &format!("{marker} 原生@验收：只提醒发起测试的当前用户。"),
        )
        .await?;
    std::fs::write(
        root.join("ready.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"anchor_message_id":anchor,"thread_id":thread}),
        )?,
    )?;
    let message = tokio::time::timeout(std::time::Duration::from_secs(180), async {
        while let Some(event) = incoming.recv().await {
            if let FeishuEvent::MessageReceived(message) = event {
                if message.chat_id == chat && message.text.contains(&format!("{marker}_REQUEST")) {
                    return Ok(message);
                }
            }
        }
        Err(anyhow::anyhow!("WebSocket closed"))
    })
    .await??;
    anyhow::ensure!(message.at_bot, "user must address bot");
    let request_id = message.message_id.clone();
    let sender_id = message.sender_user_id.clone();
    let channel_id = feishu_session_channel_id(&message);
    tokio::time::timeout(
        std::time::Duration::from_secs(180),
        process_feishu_message(
            runtime.clone(),
            gateway.clone(),
            "feishu".into(),
            message,
            FeishuActiveRuns::default(),
        ),
    )
    .await??;
    let session_id = runtime
        .sessions
        .lock()
        .await
        .channel_session_id("feishu", &channel_id)
        .ok_or_else(|| anyhow::anyhow!("missing session"))?;
    std::fs::write(
        root.join("history.json"),
        serde_json::to_vec_pretty(&runtime.bot.thread_history(&session_id).await)?,
    )?;
    // Force invalid model output through the same renderer and verify the
    // platform receives only the rejection, without a fabricated mention.
    let blocked_protocol = FeishuOutputProtocol {
        context: OutputProtocolContext::new("feishu", &channel_id, "group", "a0"),
        platform_user_ids: HashMap::new(),
        workspace: root.clone(),
    };
    let blocked = blocked_protocol.render(&format!("@{sender_id} invalid mention"));
    anyhow::ensure!(blocked.contains("已拦截") && !blocked.contains('@'));
    let blocked_id = gateway
        .reply_final_text(
            &request_id,
            &format!("{marker}_BLOCKED {blocked}"),
            &format!("{marker}-blocked"),
        )
        .await?;
    std::fs::write(
        root.join("blocked.json"),
        serde_json::to_vec(&serde_json::json!({"message_id":blocked_id}))?,
    )?;
    let fallback = super::mentions::card_mentions_for_text(&format!(
        "<at id={sender_id}></at> {marker}_TEXT_FALLBACK"
    ));
    let fallback_id = gateway
        .reply_final_text(&request_id, &fallback, &format!("{marker}-text"))
        .await?;
    std::fs::write(
        root.join("text-fallback.json"),
        serde_json::to_vec(&serde_json::json!({"message_id":fallback_id}))?,
    )?;
    std::fs::write(
        root.join("handler-done.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"websocket_ingress":true,"request_message_id":request_id,"sender_user_id":sender_id,"session_id":session_id}),
        )?,
    )?;
    Ok(())
}
