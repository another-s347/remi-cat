//! Explicitly authorized real Feishu regression acceptance; no synthetic ingress.
use super::*;
use bot_core::{im_tools::ImFileBridge, CatBotBuilder};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

fn save(root: &Path, name: &str, value: &Value) -> anyhow::Result<()> {
    std::fs::write(root.join(name), serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires authorization: real Feishu user messages, model calls, fork topic and worker channel"]
async fn live_review_fork_child_and_delivery_regressions() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("im_feishu=info,remi_cat=info,bot_core=warn")
        .with_ansi(false)
        .without_time()
        .try_init();
    tokio::task::LocalSet::new().run_until(async {
        let root = PathBuf::from(std::env::var("REMI_REVIEW_LIVE_DIR")?);
        let chat_id = std::env::var("REMI_FEISHU_IMAGE_LIVE_CHAT")?;
        let marker = std::fs::read_to_string(root.join("marker.txt"))?;
        let credentials = std::collections::BTreeMap::from([("KIMI_API_KEY".into(), std::env::var("KIMI_API_KEY")?)]);
        let worker = crate::ApplicationBuilder::new(root.join("worker-data"))
            .model_source(&root).workspace(std::env::current_dir()?).default_agent("worker").default_model("kimi-k2.6")
            .credentials(credentials.clone()).discover_hooks(false).include_default_skills(false).file_skills(false).spawn().await?;
        let gateway = FeishuGateway::new(std::env::var("FEISHU_APP_ID")?, std::env::var("FEISHU_APP_SECRET")?);
        let bridge = Arc::new(LocalImFileBridge::with_gateways(HashMap::from([("feishu".into(),gateway.clone())]), true)
            .with_topic_anchors_path(root.join("topic-anchors.json"))?);
        let data_dir = root.join("data"); std::fs::create_dir_all(&data_dir)?;
        let runtime = Rc::new(Runtime {
            bot: Rc::new(CatBotBuilder::from_env()?.im_bridge(bridge.clone())
                .workspace(std::env::current_dir()?)
                .a2a_delegate_transport(Arc::new(crate::a2a_channel::ApplicationMemoryA2aTransport::new(worker.handle()))).build()?),
            secret_store: Arc::new(Mutex::new(crate::secret_store::SecretStore::dotenv(root.join("unused.env")))),
            user_store: Arc::new(UserStore::load(&data_dir.join("users.json"))?),
            sessions: Arc::new(Mutex::new(crate::session::SessionRuntime::load(&data_dir)?)),
            im_bridge: bridge.clone(), root_agent_id: "default".into(), data_dir: data_dir.clone(),
        });
        let mut incoming = gateway.start().await?;
        let (anchor, thread_id) = gateway.send_topic_text(&chat_id, &format!("{marker} 修复验收：图文附件、fork回放、子会话续聊与交付失败上报。")).await?;
        save(&root,"ready.json", &json!({"chat_id":chat_id,"anchor_message_id":anchor,"thread_id":thread_id,"marker":marker}))?;
        println!("REVIEW_SUITE_READY");
        let mut source_session = String::new();
        let mut child_id = String::new();
        let mut fork_id = String::new();
        let mut evidence = json!({"marker":marker,"websocket_ingress":true,"stages":[]});
        for stage in ["resources", "fork", "delegate", "child-continue", "fork-continue"] {
            let message = tokio::time::timeout(std::time::Duration::from_secs(600), async {
                while let Some(event) = incoming.recv().await {
                    if let FeishuEvent::MessageReceived(message) = event {
                        if message.text.contains(&format!("{marker}_{stage}")) {return Ok(message);}
                    }
                }
                Err(anyhow::anyhow!("WebSocket closed"))
            }).await??;
            anyhow::ensure!(message.at_bot,"test user must address bot");
            let request_id = message.message_id.clone();
            let channel_id = feishu_session_channel_id(&message);
            save(&root,&format!("incoming-{stage}.json"), &json!({"message_id":request_id,"chat_id":message.chat_id,"thread_id":message.thread_id,"text":message.text,"sender_user_id":message.sender_user_id}))?;
            println!("REVIEW_RECEIVED {stage}");
            tokio::time::timeout(std::time::Duration::from_secs(300), process_feishu_message(runtime.clone(),gateway.clone(),"feishu".into(),message.clone(),FeishuActiveRuns::default())).await??;
            let session_id = runtime.sessions.lock().await.channel_session_id("feishu", &channel_id).ok_or_else(|| anyhow::anyhow!("session missing"))?;
            let mut stage_result = json!({"stage":stage,"request_message_id":request_id,"session_id":session_id,"chat_id":message.chat_id});
            if stage == "resources" {source_session = session_id.clone();}
            if stage == "fork" {
                let fork = runtime.sessions.lock().await.list().into_iter().find(|session| session.title.as_deref().is_some_and(|title| title.contains(&format!("{marker}_fork")))).ok_or_else(|| anyhow::anyhow!("fork session missing"))?;
                fork_id = fork.id.clone();
                let reply_anchor = bridge.sub_session_reply_anchor("feishu", &fork.channel_binding.channel_id).await.ok_or_else(|| anyhow::anyhow!("fork anchor missing"))?;
                stage_result["fork"] = json!({"session_id":fork.id,"channel_id":fork.channel_binding.channel_id,"anchor_message_id":reply_anchor});
                let source = runtime.bot.thread_history(&source_session).await;
                let fork_history = runtime.bot.thread_history(&fork_id).await;
                anyhow::ensure!(crate::sub_session::transcript(&source)==crate::sub_session::transcript(&fork_history),"fork history differs");
                stage_result["history_equal"] = true.into();
            }
            if stage == "delegate" {
                let parent = runtime.sessions.lock().await.get(&source_session).unwrap();
                let child = parent.sub_sessions.iter().find(|child| child.target=="worker").ok_or_else(|| anyhow::anyhow!("real worker not invoked"))?;
                anyhow::ensure!(child.status=="done","worker did not finish: {}",child.status);
                let binding = child.channel_binding.clone().ok_or_else(|| anyhow::anyhow!("worker channel not bound"))?;
                child_id = runtime.sessions.lock().await.channel_session_id(&binding.platform,&binding.channel_id).ok_or_else(|| anyhow::anyhow!("child session missing"))?;
                stage_result["child"] = json!({"session_id":child_id,"chat_id":binding.channel_id,"thread_id":child.id});
                let history = runtime.bot.thread_history(&child_id).await;
                let expected = std::fs::read_to_string(root.join("report.txt"))?;
                let combined = history.iter().filter(|message|message.role=="assistant").map(|message|message.text.as_str()).collect::<String>();
                anyhow::ensure!(combined.contains(expected.trim_end()),"worker history lost whitespace: {combined:?}");
                stage_result["whitespace_preserved"] = true.into();
            }
            if stage == "child-continue" {
                anyhow::ensure!(session_id==child_id,"continuation reached wrong session");
                let history = runtime.bot.thread_history(&child_id).await;
                anyhow::ensure!(history.iter().any(|message|message.role=="assistant" && message.text.contains(&format!("{marker}_CONTINUED"))),"followup final missing");
                save(&root,"child-history.json", &serde_json::to_value(&history)?)?;
            }
            if stage == "fork-continue" {anyhow::ensure!(session_id==fork_id,"fork continuation reached wrong session");}
            evidence["stages"].as_array_mut().unwrap().push(stage_result.clone());
            save(&root,&format!("done-{stage}.json"), &stage_result)?;
            println!("REVIEW_COMPLETED {stage}");
        }
        // Exercise the actual API rejection through the same checked final
        // commit used by fork replay. This request intentionally cannot send.
        let invalid_anchor = format!("om_review_missing_{}",uuid::Uuid::new_v4().simple());
        let invalid_message = FeishuMessage {message_id:invalid_anchor.clone(),sender_user_id:"live-test".into(),chat_id:chat_id.clone(),chat_type:"group".into(),text:String::new(),images:vec![],files:vec![],documents:vec![],parent_id:None,thread_id:None,at_bot:true,mentions:vec![]};
        let protocol = build_feishu_output_protocol(&runtime,"feishu",&source_session,&invalid_message,None,Some(&gateway)).await;
        let mut replies = FeishuReplyStream::new(gateway.clone(),chat_id.clone(),invalid_anchor.clone());
        let error = {
        let mut reply_option = Some(&mut replies);
        let mut forwarder = FeishuEventForwarder {runtime:&runtime,platform:"feishu",msg:&invalid_message,session_id:&source_session,debug_enabled:false,gateway:Some(&gateway),replies:&mut reply_option,state:FeishuForwarderState::new(protocol,invalid_anchor)};
        forwarder.forward_core_event(CoreChatEvent::ResponseCompleted {message_id:"failure-probe".into(),text:format!("{marker} expected delivery rejection")}).await;
        forwarder.commit_final_output().await.expect_err("real Feishu rejection must propagate")
        };
        evidence["real_api_delivery_failure_propagated"] = true.into();
        evidence["delivery_failure"] = format!("{error:#}").into();
        anyhow::ensure!(replies.delivery_result().is_err(),"failure state missing");
        replies.finish().await;
        anyhow::ensure!(replies.delivery_result().is_err(),"finish swallowed failure");
        drop(runtime);
        // Reopen the actual persisted Feishu child through the Application/TUI
        // loader, including the direct followup that has no SubSession log.
        let application = crate::ApplicationBuilder::new(&data_dir).model_source(&root).workspace(std::env::current_dir()?)
            .default_model("kimi-k2.6").credentials(credentials).discover_hooks(false).include_default_skills(false).file_skills(false).spawn().await?;
        let channel = application.handle().channel(crate::ChannelConfig::new("feishu"));
        let view = crate::tui_app::capture_live_sub_session_view(channel,child_id).await;
        let view_text = view.to_string();
        anyhow::ensure!(view_text.contains(&format!("{marker}_WORKER")) && view_text.contains(&format!("{marker}_CONTINUED")),"reopened view lost messages: {view}");
        anyhow::ensure!(!view["tool_ids"].as_array().unwrap().is_empty(),"reopened view lost tool cells");
        evidence["reopened_actual_child_view"] = view;
        evidence["status"] = "PASS".into();
        save(&root,"results.json", &evidence)?;
        application.shutdown().await?;worker.shutdown().await?;
        println!("REVIEW_SUITE_PASS");
        Ok::<_,anyhow::Error>(())
    }).await
}

#[tokio::test]
#[ignore = "calls the actual Feishu API with an intentionally invalid message anchor and posts acceptance result"]
async fn live_review_delivery_rejection_is_reported() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::var("REMI_REVIEW_LIVE_DIR")?);
    let marker = std::fs::read_to_string(root.join("marker.txt"))?;
    let chat = std::env::var("REMI_FEISHU_IMAGE_LIVE_CHAT")?;
    let gateway = FeishuGateway::new(
        std::env::var("FEISHU_APP_ID")?,
        std::env::var("FEISHU_APP_SECRET")?,
    );
    gateway.ensure_authenticated().await?;
    let mut replies = FeishuReplyStream::new(
        gateway.clone(),
        chat.clone(),
        format!("om_review_missing_{}", uuid::Uuid::new_v4().simple()),
    );
    replies.set_pending_final_text(format!("{marker}: expected API rejection probe"));
    let error = replies
        .commit_final_output()
        .await
        .expect_err("actual API rejection must be returned");
    anyhow::ensure!(replies.delivery_result().is_err());
    replies.finish().await;
    anyhow::ensure!(replies.delivery_result().is_err());
    let receipt = gateway.send_text(&chat,&format!("{marker} 交付失败回归通过：用无效消息ID调用真实飞书API，最终发送明确返回错误，finish后仍保留失败状态，没有误报成功。正常消息发送也已验证。" )).await?;
    save(
        &root,
        "delivery-rejection.json",
        &json!({"status":"PASS","actual_api_error":format!("{error:#}"),"failure_survived_finish":true,"result_message_id":receipt}),
    )?;
    println!("REAL_DELIVERY_REJECTION_PASS {receipt}");
    Ok(())
}
