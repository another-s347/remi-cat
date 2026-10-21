//! Opt-in real user/WebSocket/LLM/Feishu async delivery acceptance.
use super::*;
use bot_core::CatBotBuilder;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires authorized real Feishu user messages, a real model and a local background command"]
async fn live_async_foreground_ping_and_background_delivery() -> anyhow::Result<()> {
    tokio::task::LocalSet::new()
        .run_until(run_live_async_acceptance())
        .await
}

// Default mode uses real user WebSocket ingress. The optional bot-only probe
// explicitly records synthetic ingress while exercising real model/tool/API I/O.
async fn run_live_async_acceptance() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::var("REMI_ASYNC_LIVE_DIR")?);
    let marker = std::env::var("REMI_ASYNC_LIVE_MARKER")?;
    let chat = std::env::var("REMI_ASYNC_LIVE_CHAT")?;
    anyhow::ensure!(async_agent_enabled(), "async mode must be enabled");
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
                .workspace(root.clone())
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
            &format!("{marker} 异步验收：真实长工具、前台先回复、等待期间续聊、后台完成再回复。"),
        )
        .await?;
    std::fs::write(
        root.join("ready.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"chat_id":chat,"anchor_message_id":anchor,"thread_id":thread}),
        )?,
    )?;
    println!("ASYNC_LIVE_READY");
    let direct_probe = std::env::var("REMI_ASYNC_PROBE_IDENTITY").as_deref() == Ok("bot");
    let active_runs = FeishuActiveRuns::default();
    let mut handles = Vec::new();
    for stage in ["START", "PING"] {
        let message = if direct_probe {
            // The bot-only smoke test explicitly bypasses user ingress.
            // The external driver creates a real bot message first, then
            // feeds that receipt to the production handler as test input.
            let receipt = root.join(format!("sent-{}.json", stage.to_ascii_lowercase()));
            let sent = tokio::time::timeout(std::time::Duration::from_secs(240), async {
                while !receipt.exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(&receipt)?)
                    .map_err(anyhow::Error::from)
            })
            .await??;
            FeishuMessage {
                message_id: sent["data"]["message_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("bot receipt missing message ID"))?
                    .into(),
                sender_user_id: "explicit-bot-only-acceptance".into(),
                chat_id: chat.clone(),
                chat_type: "group".into(),
                text: std::fs::read_to_string(
                    root.join(format!("{}-prompt.txt", stage.to_ascii_lowercase())),
                )?,
                images: vec![],
                files: vec![],
                documents: vec![],
                parent_id: Some(anchor.clone()),
                thread_id: Some(thread.clone()),
                at_bot: true,
                mentions: vec![],
            }
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(300), async {
                while let Some(event) = incoming.recv().await {
                    if let FeishuEvent::MessageReceived(message) = event {
                        if message.chat_id == chat
                            && message.text.contains(&format!("{marker}_{stage}"))
                        {
                            return Ok(message);
                        }
                    }
                }
                Err(anyhow::anyhow!("WebSocket closed"))
            })
            .await??
        };
        anyhow::ensure!(message.at_bot, "real user must address bot");
        let session_id = runtime.sessions.lock().await.resolve_channel(
            "feishu",
            &feishu_session_channel_id(&message),
            &runtime.root_agent_id,
        )?;
        // This isolated acceptance session only runs the explicitly requested
        // test script; it does not change a user's persistent permissions.
        runtime
            .bot
            .approval_manager()
            .set_session_policy(
                &session_id,
                bot_core::approval::ApprovalSessionPolicy::Medium,
            )
            .await;
        std::fs::write(
            root.join(format!("incoming-{stage}.json")),
            serde_json::to_vec_pretty(
                &serde_json::json!({"message_id":message.message_id,"sender_user_id":message.sender_user_id,"thread_id":message.thread_id,"text":message.text,"session_id":session_id}),
            )?,
        )?;
        let runtime = runtime.clone();
        let gateway = gateway.clone();
        let runs = active_runs.clone();
        handles.push(tokio::task::spawn_local(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(240),
                process_feishu_message(runtime, gateway, "feishu".into(), message, runs),
            )
            .await?
        }));
    }
    for handle in handles {
        handle.await??;
    }
    anyhow::ensure!(
        root.join("tool-finished.json").exists(),
        "real tool never finished"
    );
    std::fs::write(
        root.join("handler-done.json"),
        serde_json::to_vec(
            &serde_json::json!({"status":"PASS","websocket_ingress":!direct_probe,"synthetic_user_input":direct_probe,"real_model":true}),
        )?,
    )?;
    Ok(())
}
