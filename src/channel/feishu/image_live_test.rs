//! Opt-in live verification. The caller must authorize the target chat first.
//! REMI_FEISHU_IMAGE_LIVE_DIR contains probe.png and expected.json, kept out of
//! the model prompt. Credentials are read from the process environment only.
use super::*;
use bot_core::{im_tools::ImFileBridge, CatBotBuilder};
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test(flavor = "current_thread")]
#[ignore = "sends messages to an explicitly authorized Feishu test chat and calls a real model"]
async fn live_image_download_model_readback_and_native_reply() -> anyhow::Result<()> {
    tokio::task::LocalSet::new().run_until(async {
        let root = PathBuf::from(std::env::var("REMI_FEISHU_IMAGE_LIVE_DIR")?);
        let chat_id = std::env::var("REMI_FEISHU_IMAGE_LIVE_CHAT")?;
        let gateway = FeishuGateway::new(
            std::env::var("FEISHU_APP_ID")?, std::env::var("FEISHU_APP_SECRET")?);
        gateway.ensure_authenticated().await?;
        let bridge = Arc::new(LocalImFileBridge::new(Some(gateway.clone())));
        let bytes = tokio::fs::read(root.join("probe.png")).await?;
        // Exercise the same bridge used by im_upload, including MIME inference.
        let sent = bridge.upload(ImUploadRequest {
            platform: "feishu".into(), message_id: String::new(), chat_id: chat_id.clone(),
            file_name: "probe.png".into(), mime_type: "image/png".into(),
            content: bytes.clone(), file_type: String::new(),
        }).await?;
        let parent = gateway.fetch_parent_content(&sent.message_id).await?.expect("sent image exists");
        assert_eq!(parent.images, vec![sent.file_key.clone()], "must be an image message, not a file");
        assert!(parent.files.is_empty());
        let mut msg = FeishuMessage {
            message_id: sent.message_id.clone(), sender_user_id: "image-live-probe".into(),
            chat_id: chat_id.clone(), chat_type: "group".into(),
            text: "Read the image. Report the exact alphanumeric code and the three block colors from left to right in English. Then display the supplied image using ![verified](remi-resource:target/feishu-image-e2e-20261018/probe.png). Do not use tools.".into(),
            images: parent.images, files: vec![], documents: vec![], parent_id: None,
            thread_id: None, at_bot: true, mentions: vec![],
        };
        let prepared = prepare_feishu_input(Some(&gateway), &msg).await;
        assert!(prepared.had_images);
        assert_eq!(prepared.image_urls.len(), 1);
        let encoded = prepared.image_urls[0].split_once(',').unwrap().1;
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(encoded)?, bytes);
        // Also verify quoted-image owner IDs against the real message API.
        msg.images.clear();
        msg.parent_id = Some(sent.message_id.clone());
        let quoted = prepare_feishu_input(Some(&gateway), &msg).await;
        assert_eq!(quoted.image_urls, prepared.image_urls);
        msg.parent_id = None;
        msg.images = vec![sent.file_key.clone()];
        let rich_message_id = std::env::var("REMI_FEISHU_IMAGE_LIVE_RICH_MESSAGE")?;
        let rich = gateway.fetch_parent_content(&rich_message_id).await?.expect("rich-text probe exists");
        assert_eq!(rich.images.len(), 1);
        let mut rich_msg = msg.clone();
        rich_msg.message_id = rich_message_id.clone();
        rich_msg.images = rich.images;
        rich_msg.text = rich.text.unwrap_or_default();
        let rich_input = prepare_feishu_input(Some(&gateway), &rich_msg).await;
        assert_eq!(rich_input.image_urls, prepared.image_urls);

        let data_dir = root.join("data");
        std::fs::create_dir_all(&data_dir)?;
        let runtime = Rc::new(Runtime {
            bot: Rc::new(CatBotBuilder::from_env()?.im_bridge(bridge.clone()).build()?),
            secret_store: Arc::new(Mutex::new(crate::secret_store::SecretStore::dotenv(root.join("unused.env")))),
            user_store: Arc::new(UserStore::load(&data_dir.join("users.json"))?),
            sessions: Arc::new(Mutex::new(crate::session::SessionRuntime::load(&data_dir)?)),
            im_bridge: bridge, root_agent_id: "default".into(), data_dir,
        });
        let protocol = build_feishu_output_protocol(&runtime, "feishu", "image-live", &msg, None, Some(&gateway)).await;
        let remote = serve_image_once(bytes.clone(), "image/png").await;
        let raw_markdown = format!("INLINE_BEFORE\n\n![local]({})\n\nINLINE_BETWEEN\n\n![remote]({remote})\n\nINLINE_AFTER", root.join("probe.png").display());
        let rendered = protocol.render_final(&runtime, &msg, &raw_markdown, Some(&gateway)).await;
        assert_eq!(rendered.matches("](img_").count(), 2, "{rendered}");
        assert!(!rendered.contains("已发送"));
        let card: serde_json::Value = serde_json::from_str(&im_feishu::client::card_content(&rendered))?;
        let elements = card["body"]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 5);
        assert_eq!(elements[1]["tag"], "img");
        assert_eq!(elements[3]["tag"], "img");
        let delivery_id = uuid::Uuid::new_v4().to_string();
        let inline_card_id = gateway.reply_final_card(&msg.message_id, &rendered, &delivery_id).await?;
        let repeated_id = gateway.reply_final_card(&msg.message_id, &rendered, &delivery_id).await?;
        assert_eq!(inline_card_id, repeated_id, "retry UUID must not create a duplicate card");
        let card_body = fetch_original_card(&inline_card_id).await?;
        std::fs::write(root.join("inline-card-readback.json"), serde_json::to_vec_pretty(&card_body)?)?;
        let actual_elements = card_body["body"]["elements"].as_array().expect("original card elements");
        assert_eq!(actual_elements.len(), 5);
        for index in 0..5 {
            assert_eq!(actual_elements[index]["tag"], elements[index]["tag"]);
        }
        for index in [1, 3] {
            let key = elements[index]["img_key"].as_str().unwrap();
            assert_eq!(actual_elements[index]["img_key"], key);
            assert_eq!(gateway.download_image(&inline_card_id, key).await?.1, bytes);
        }
        std::fs::write(root.join("inline-card-payload.json"), serde_json::to_vec_pretty(&card)?)?;
        // Exercise production Card 2.0 readback and then ask the real model
        // to read the images obtained from the quoted card, not the input image.
        let parent = gateway.fetch_parent_content(&inline_card_id).await?.expect("quoted card exists");
        assert_eq!(parent.images.len(), 2);
        let quoted_text = parent.text.as_deref().unwrap_or("");
        assert!(quoted_text.contains("INLINE_BEFORE") && quoted_text.contains("INLINE_AFTER"));
        assert!(!quoted_text.contains("请升级"));
        msg.images.clear();
        msg.parent_id = Some(inline_card_id.clone());
        let prepared_card = prepare_feishu_input(Some(&gateway), &msg).await;
        assert_eq!(prepared_card.image_urls.len(), 2);
        let mut replies = FeishuReplyStream::new(gateway.clone(), chat_id, sent.message_id.clone());
        let output = tokio::time::timeout(std::time::Duration::from_secs(240), collect_bot_reply(
            runtime, "feishu", msg, Some("Image verification".into()), None,
            Some(&gateway), Some(&mut replies),
        )).await??;
        replies.finish().await;
        let expected: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("expected.json"))?)?;
        std::fs::write(root.join("model-reply.txt"), &output)?;
        assert!(output.contains(expected["code"].as_str().unwrap()), "model must read the unseen code: {output}");
        let lower = output.to_ascii_lowercase();
        let red = lower.find("red").expect("red block");
        let green = lower.find("green").expect("green block");
        let blue = lower.find("blue").expect("blue block");
        assert!(red < green && green < blue, "wrong color order: {output}");
        assert!(output.contains("remi-resource:"), "model must return a resource image");
        let evidence = serde_json::json!({
            "input_message_id": sent.message_id, "input_image_key": sent.file_key,
            "download_byte_equality": true, "quoted_image_byte_equality": true,
            "rich_text_download_byte_equality": true, "rich_text_message_id": rich_message_id,
            "model_code_match": true, "model_color_order_match": true,
            "ordinary_local_markdown_image_embedded": true, "http_markdown_image_embedded": true,
            "inline_card_message_id": inline_card_id, "inline_card_image_count": 2,
            "inline_card_order": ["markdown", "img", "markdown", "img", "markdown"],
            "inline_card_readback_image_keys_match": true, "inline_card_images_byte_identical": true,
            "stable_uuid_returns_same_message_id": true, "quoted_card_image_count": 2,
            "quoted_card_text_preserved": true, "model_input_source": "images downloaded from quoted Card 2.0",
            "boundary": "real Feishu upload/read/download, production prepare/chat/final-render paths; no WebSocket event delivery claim"
        });
        std::fs::write(root.join("inline-results.json"), serde_json::to_vec_pretty(&evidence)?)?;
        println!("{}", serde_json::to_string_pretty(&evidence)?);
        Ok::<_, anyhow::Error>(())
    }).await
}

#[test]
fn markdown_images_are_uploaded_but_links_and_code_are_preserved() {
    let context = OutputProtocolContext::new("feishu", "test", "group", "a0");
    let text = "![a](local.png) ![b](https://example.com/b.png) [link](c.png) `![code](d.png)`\n\n```md\n![code](e.png)\n```";
    let doc = parse_feishu_image_output(text, &context);
    assert_eq!(doc.nodes.len(), 2);
    for node in doc.nodes {
        assert!(matches!(
            node.kind,
            OutputNodeKind::Resource { image: true, .. }
        ));
        assert!(text[node.start..node.end].starts_with("!["));
    }
}

async fn serve_image_once(bytes: Vec<u8>, mime: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        stream.read(&mut request).await.unwrap();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).as_bytes()).await.unwrap();
        stream.write_all(&bytes).await.unwrap();
    });
    format!("http://{address}/probe.png")
}

#[tokio::test]
async fn remote_image_loader_downloads_images_and_rejects_html() {
    let url = serve_image_once(vec![1, 2, 3], "image/png").await;
    let (name, mime, bytes) = load_feishu_output_resource(&url, true, Path::new("."))
        .await
        .unwrap();
    assert_eq!(name, "probe.png");
    assert_eq!(mime, "image/png");
    assert_eq!(bytes, vec![1, 2, 3]);
    let url = serve_image_once(b"not an image".to_vec(), "text/html").await;
    assert!(load_feishu_output_resource(&url, true, Path::new("."))
        .await
        .is_err());
}

// The default message readback is a legacy-client fallback, so request the
// original Card 2.0 payload explicitly when verifying image placement.
async fn fetch_original_card(message_id: &str) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let auth: serde_json::Value = client.post("https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal")
        .json(&serde_json::json!({"app_id": std::env::var("FEISHU_APP_ID")?, "app_secret": std::env::var("FEISHU_APP_SECRET")?}))
        .send().await?.error_for_status()?.json().await?;
    let token = auth["tenant_access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("live readback authentication failed"))?;
    let result: serde_json::Value = client
        .get(format!(
            "https://open.feishu.cn/open-apis/im/v1/messages/{message_id}"
        ))
        .query(&[("card_msg_content_type", "user_card_content")])
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    anyhow::ensure!(
        result["code"] == 0,
        "card readback failed: {}",
        result["msg"]
    );
    let item = &result["data"]["items"][0];
    anyhow::ensure!(
        item["msg_type"] == "interactive",
        "expected interactive card"
    );
    Ok(serde_json::from_str(
        item["body"]["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing card body"))?,
    )?)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "sends native table/chart cards and calls a real model in the authorized test chat"]
async fn live_structured_table_chart_model_roundtrip() -> anyhow::Result<()> {
    tokio::task::LocalSet::new().run_until(async {
        let root = PathBuf::from(std::env::var("REMI_FEISHU_IMAGE_LIVE_DIR")?);
        let chat_id = std::env::var("REMI_FEISHU_IMAGE_LIVE_CHAT")?;
        let gateway = FeishuGateway::new(std::env::var("FEISHU_APP_ID")?, std::env::var("FEISHU_APP_SECRET")?);
        gateway.ensure_authenticated().await?;
        let random = uuid::Uuid::new_v4();
        let first = 10 + u64::from(random.as_bytes()[0]);
        let second = 300 + u64::from(random.as_bytes()[1]);
        let table = serde_json::json!({"columns":[{"id":"name","label":"项目"},{"id":"value","label":"数值","kind":"number"}],"rows":[{"name":"Alpha","value":first},{"name":"Beta","value":second}],"page_size":1});
        let mut source = format!("STRUCTURED_BEFORE\n\n```remi-table\n{table}\n```\n\nSTRUCTURED_MIDDLE\n\n");
        for kind in ["bar", "line", "pie"] {
            let chart = serde_json::json!({"kind":kind,"points":[{"label":"Alpha","value":first},{"label":"Beta","value":second}]});
            source.push_str(&format!("```remi-chart\n{chart}\n```\n\n"));
        }
        source.push_str("STRUCTURED_AFTER");
        let anchor = gateway.send_text(&chat_id, "结构化表格与图表验收：原生表格、柱状图、折线图、饼图及引用回读。").await?;
        let fixture_id = gateway.reply_final_card(&anchor, &source, &random.to_string()).await?;
        let card = fetch_original_card(&fixture_id).await?;
        let elements = card["body"]["elements"].as_array().expect("native elements");
        assert_eq!(elements.iter().filter(|v| v["tag"] == "table").count(), 1);
        assert_eq!(elements.iter().filter(|v| v["tag"] == "chart").count(), 3);
        let actual_table = elements.iter().find(|v| v["tag"] == "table").unwrap();
        assert_eq!(actual_table["page_size"], 1);
        assert_eq!(actual_table["rows"][0]["value"], first);
        let quote = gateway.fetch_parent_content(&fixture_id).await?.expect("quoted structured card");
        let text = quote.text.unwrap_or_default();
        assert!(text.contains(&format!("Alpha | {first}")));
        assert!(text.contains(&format!("Beta | {second}")));
        assert!(!text.contains("请升级"));
        let data_dir = root.join("structured-data"); std::fs::create_dir_all(&data_dir)?;
        let bridge = Arc::new(LocalImFileBridge::new(Some(gateway.clone())));
        let runtime = Rc::new(Runtime {
            bot: Rc::new(CatBotBuilder::from_env()?.im_bridge(bridge.clone()).build()?),
            secret_store: Arc::new(Mutex::new(crate::secret_store::SecretStore::dotenv(root.join("unused.env")))),
            user_store: Arc::new(UserStore::load(&data_dir.join("users.json"))?),
            sessions: Arc::new(Mutex::new(crate::session::SessionRuntime::load(&data_dir)?)),
            im_bridge: bridge, root_agent_id: "default".into(), data_dir,
        });
        let mut message = FeishuMessage { message_id: String::new(), sender_user_id: "structured-live-probe".into(), chat_id: String::new(), chat_type: "group".into(), text: String::new(), images: vec![], files: vec![], documents: vec![], parent_id: None, thread_id: None, at_bot: true, mentions: vec![] };
        message.message_id = anchor.clone(); message.chat_id = chat_id.clone();
        message.parent_id = Some(fixture_id.clone());
        message.text = "请读取引用卡片中 Alpha 和 Beta 的实际数值。不要使用工具。先用 remi-table JSON 代码围栏输出一张表格，列 id 为 name（text）和 value（number），保留两项原始数值；再用 remi-chart JSON 代码围栏输出柱状图(kind=bar)，points 的 label 和 value 使用相同原始数据。前后可以有简短文字。不要输出飞书组件JSON，不要将围栏放进其他代码块。".into();
        let mut replies = FeishuReplyStream::new(gateway.clone(), chat_id, anchor.clone());
        let output = tokio::time::timeout(std::time::Duration::from_secs(240), collect_bot_reply(runtime, "feishu", message, Some("Structured verification".into()), None, Some(&gateway), Some(&mut replies))).await??;
        replies.finish().await; replies.delivery_result()?;
        let spans = remi_output_protocol::structured_spans(&output);
        assert!(spans.iter().any(|span| matches!(&span.result, Ok(remi_output_protocol::StructuredOutput::Table(table)) if table.rows.iter().any(|row| row.get("value") == Some(&serde_json::json!(first))))));
        assert!(spans.iter().any(|span| matches!(&span.result, Ok(remi_output_protocol::StructuredOutput::Chart(chart)) if chart.points.iter().any(|point| point.value.as_u64() == Some(second)))));
        std::fs::write(root.join("structured-fixture-card.json"), serde_json::to_vec_pretty(&card)?)?;
        let evidence = serde_json::json!({"anchor_message_id":anchor,"fixture_card_message_id":fixture_id,"expected_values":{"Alpha":first,"Beta":second},"native_table_count":1,"native_chart_kinds":["bar","line","pie"],"native_table_page_size":1,"quoted_card_data_preserved":true,"real_model_structured_output":true,"boundary":"real Feishu API and real model; no physical client display or WebSocket ingress claim"});
        std::fs::write(root.join("structured-results.json"), serde_json::to_vec_pretty(&evidence)?)?;
        println!("{}", serde_json::to_string_pretty(&evidence)?);
        Ok::<_,anyhow::Error>(())
    }).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "waits for an authorized real Feishu user request over WebSocket and sends a real agent image reply"]
async fn live_user_websocket_request_sends_local_image() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("im_feishu=info,remi_cat=info,bot_core=warn")
        .with_ansi(false)
        .without_time()
        .try_init();
    tokio::task::LocalSet::new().run_until(async {
        let root = PathBuf::from(std::env::var("REMI_FEISHU_IMAGE_LIVE_DIR")?);
        let chat_id = std::env::var("REMI_FEISHU_IMAGE_LIVE_CHAT")?;
        let marker = std::env::var("REMI_FEISHU_USER_IMAGE_MARKER")?;
        let image_path = root.join("picture.png");
        anyhow::ensure!(image_path.is_file(), "test image must exist");
        let gateway = FeishuGateway::new(std::env::var("FEISHU_APP_ID")?, std::env::var("FEISHU_APP_SECRET")?);
        let bridge = Arc::new(LocalImFileBridge::new(Some(gateway.clone())));
        let data_dir = root.join("data"); std::fs::create_dir_all(&data_dir)?;
        let runtime = Rc::new(Runtime {
            bot: Rc::new(CatBotBuilder::from_env()?.im_bridge(bridge.clone()).build()?),
            secret_store: Arc::new(Mutex::new(crate::secret_store::SecretStore::dotenv(root.join("unused.env")))),
            user_store: Arc::new(UserStore::load(&data_dir.join("users.json"))?),
            sessions: Arc::new(Mutex::new(crate::session::SessionRuntime::load(&data_dir)?)),
            im_bridge: bridge, root_agent_id: "default".into(), data_dir,
        });
        let mut incoming = gateway.start().await?;
        println!("IMAGE_SEND_WS_WAITING marker={marker}");
        let message = tokio::time::timeout(std::time::Duration::from_secs(180), async {
            while let Some(event) = incoming.recv().await {
                if let FeishuEvent::MessageReceived(message) = event {
                    if message.chat_id == chat_id && message.text.contains(&marker) { return Ok(message); }
                }
            }
            Err(anyhow::anyhow!("Feishu WebSocket event receiver closed"))
        }).await??;
        anyhow::ensure!(message.at_bot, "user request must address this bot");
        anyhow::ensure!(message.text.contains(&image_path.display().to_string()), "user request must identify the local image");
        let message_id = message.message_id.clone();
        let sender_user_id = message.sender_user_id.clone();
        let text = message.text.clone();
        std::fs::write(root.join("incoming-user-event.json"), serde_json::to_vec_pretty(&serde_json::json!({"message_id": message_id,"sender_user_id":sender_user_id,"text":text,"at_bot":message.at_bot,"transport":"real WebSocket event"}))?)?;
        println!("IMAGE_SEND_WS_RECEIVED message_id={message_id}");
        tokio::time::timeout(std::time::Duration::from_secs(240), process_feishu_message(runtime, gateway, "feishu".into(), message, FeishuActiveRuns::default())).await??;
        let evidence = serde_json::json!({"request_message_id":message_id,"sender_user_id":sender_user_id,"request_text":text,"image_path":image_path,"websocket_ingress":true,"production_message_handler_completed":true,"model":"kimi-k2.6","final_image_readback":"pending external verification"});
        std::fs::write(root.join("results.json"), serde_json::to_vec_pretty(&evidence)?)?;
        println!("IMAGE_SEND_HANDLER_COMPLETED message_id={message_id}");
        Ok::<_,anyhow::Error>(())
    }).await
}
