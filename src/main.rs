//! `remi-cat` — OpenClaw-style bot wiring Feishu + remi-agentloop.
//!
//! Configuration (environment variables):
//!
//! | Variable             | Required | Description                          |
//! |----------------------|----------|--------------------------------------|
//! | `FEISHU_APP_ID`      | Yes      | Feishu enterprise app ID             |
//! | `FEISHU_APP_SECRET`  | Yes      | Feishu enterprise app secret         |
//! | `OPENAI_API_KEY`     | Yes      | OpenAI-compatible API key            |
//! | `OPENAI_BASE_URL`    | No       | Custom base URL (e.g. local LLM)     |
//! | `OPENAI_MODEL`       | No       | Model name (default: gpt-4o)         |
//! | `REMI_DATA_DIR`      | No       | Data directory (default: `.remi-cat`)|

use base64::Engine as _;
use bot_core::{
    im_tools::{DownloadedImFile, ImDownloadRequest, ImFileBridge, ImUploadRequest, UploadedImFile},
    CatBotBuilder, CatEvent, Content, ContentPart, ImAttachment, ImDocument, StreamOptions,
};
use futures::StreamExt;
use im_feishu::{FeishuEvent, FeishuGateway};
use matcher::{OwnerMatcher, OwnerStatus, PAIR_COMMAND};
use std::{rc::Rc, sync::Arc, time::Duration};
use tracing::{debug, info, warn};
use user_store::UserStore;

// ── LocalImFileBridge ─────────────────────────────────────────────────────────

/// Standalone-mode bridge: calls FeishuGateway directly (no gRPC).
struct LocalImFileBridge {
    gateway: FeishuGateway,
}

impl ImFileBridge for LocalImFileBridge {
    fn download<'a>(
        &'a self,
        req: ImDownloadRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<DownloadedImFile>> + Send + 'a>> {
        Box::pin(async move {
            if req.platform != "feishu" {
                anyhow::bail!("unsupported platform: {}", req.platform);
            }
            let (mime_type, file_name, content, source_label) =
                if let Some(key) = req.attachment_key.filter(|k| !k.is_empty()) {
                    let label = format!("attachment:{key}");
                    let (mt, fn_, c) = self.gateway.download_file(&req.message_id, &key, &req.file_type).await?;
                    (mt, fn_, c, label)
                } else if let Some(url) = req.document_url.filter(|u| !u.is_empty()) {
                    let label = url.clone();
                    let (mt, fn_, c) = self.gateway.download_document(&url).await?;
                    (mt, fn_, c, label)
                } else {
                    anyhow::bail!("download request must specify attachment_key or document_url");
                };
            Ok(DownloadedImFile { file_name, mime_type, content, source_label })
        })
    }

    fn upload<'a>(
        &'a self,
        req: ImUploadRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<UploadedImFile>> + Send + 'a>> {
        Box::pin(async move {
            if req.platform != "feishu" {
                anyhow::bail!("unsupported platform: {}", req.platform);
            }
            let file_key = self
                .gateway
                .upload_file(&req.file_name, &req.mime_type, &req.content, &req.file_type)
                .await?;
            let sent_message_id = if !req.message_id.is_empty() {
                match self.gateway.reply_file(&req.message_id, &file_key, &req.file_type).await {
                    Ok(id) => id,
                    Err(e) => {
                        warn!(error = %e, "reply_file failed, falling back to send_file");
                        self.gateway.send_file(&req.chat_id, &file_key, &req.file_type).await?
                    }
                }
            } else {
                self.gateway.send_file(&req.chat_id, &file_key, &req.file_type).await?
            };
            Ok(UploadedImFile {
                file_name: req.file_name,
                file_key: file_key.clone(),
                message_id: sent_message_id.clone(),
                resource_url: self.gateway.file_resource_url(&sent_message_id, &file_key, &req.file_type),
            })
        })
    }
}

const FEISHU_CHANNEL: &str = "feishu";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "remi_cat=info,im_feishu=info".into()),
        )
        .init();

    // ── Config ────────────────────────────────────────────────────────────
    let app_id =
        std::env::var("FEISHU_APP_ID").map_err(|_| anyhow::anyhow!("FEISHU_APP_ID must be set"))?;
    let app_secret = std::env::var("FEISHU_APP_SECRET")
        .map_err(|_| anyhow::anyhow!("FEISHU_APP_SECRET must be set"))?;

    // ── Components ────────────────────────────────────────────────────────
    let data_dir = std::path::PathBuf::from(
        std::env::var("REMI_DATA_DIR").unwrap_or_else(|_| ".remi-cat".into()),
    );
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| anyhow::anyhow!("failed to create data dir {data_dir:?}: {e}"))?;

    let user_store = Arc::new(
        UserStore::load(data_dir.join("users.json"))
            .map_err(|e| anyhow::anyhow!("failed to load user store: {e:#}"))?,
    );

    let gateway = FeishuGateway::new(app_id, app_secret);
    let matcher = OwnerMatcher::load();
    let bridge: Arc<dyn ImFileBridge> = Arc::new(LocalImFileBridge { gateway: gateway.clone() });
    let bot = Rc::new(CatBotBuilder::from_env()?.im_bridge(bridge).build()?);

    info!("remi-cat starting up");
    if let Some(id) = matcher.owner_id() {
        info!("owner (uuid): {id}");
    } else {
        info!("no owner set — send '{}' to claim ownership", PAIR_COMMAND);
    }

    // ── Start gateway (spawns WS reader task, returns event channel) ──────
    let mut rx = gateway.start().await?;

    // ── LocalSet: lets spawn_local run non-Send futures on the current thread
    let local = tokio::task::LocalSet::new();
    local.run_until(async move {
    // ── Main event loop ───────────────────────────────────────────────────
    while let Some(event) = rx.recv().await {
        match event {
            FeishuEvent::MessageReceived(msg) => {
                let text = msg.text.trim().to_string();
                // Resolve Feishu open_id → internal UUID (creates entry if new).
                let sender_uuid =
                    user_store.resolve_or_create(FEISHU_CHANNEL, &msg.sender_user_id);
                info!(
                    sender = %msg.sender_user_id,
                    uuid   = %sender_uuid,
                    chat   = %msg.chat_id,
                    "received: {text}",
                );

                if matcher.is_banned(&sender_uuid) {
                    warn!("ignoring message from blacklisted user {} (uuid: {})", msg.sender_user_id, sender_uuid);
                    continue;
                }

                let sender_username =
                    ensure_im_username(&user_store, &gateway, &sender_uuid, &msg.sender_user_id)
                        .await;

                // ── Pairing command ────────────────────────────────────
                if text == PAIR_COMMAND {
                    let status = matcher.check(&sender_uuid);
                    let reply = match status {
                        OwnerStatus::NeedPairing => {
                            if matcher.try_pair(&sender_uuid) {
                                format!(
                                    "配对成功！您已成为我的主人。\nYour UUID: {}",
                                    sender_uuid
                                )
                            } else {
                                "配对失败，请重试。".into()
                            }
                        }
                        OwnerStatus::Owner => "您已是我的主人。".into(),
                        OwnerStatus::NotOwner => "我已有主人 :)".into(),
                        OwnerStatus::Banned => "我已有主人 :)".into(),
                    };
                    if let Err(e) = gateway.reply_text(&msg.message_id, &reply).await {
                        warn!("reply_text failed: {e:#}");
                        gateway.send_text(&msg.chat_id, &reply).await.ok();
                    }
                    continue;
                }

                // ── spawn_local: runs on same thread, no Send bound needed ─
                let bot = std::rc::Rc::clone(&bot);
                let gateway = gateway.clone();
                let im_attachments: Vec<ImAttachment> = msg.files.iter().map(|f| ImAttachment {
                    key: f.file_key.clone(),
                    name: f.file_name.clone(),
                    mime_type: f.mime_type.clone(),
                    size_bytes: f.size_bytes,
                    file_type: f.file_type.clone(),
                }).collect();
                let im_documents: Vec<ImDocument> = msg.documents.iter().map(|d| ImDocument {
                    url: d.url.clone(),
                    title: d.title.clone(),
                    doc_type: d.doc_type.clone(),
                    token: d.token.clone(),
                }).collect();
                let stream_opts = StreamOptions {
                    sender_user_id: Some(sender_uuid.clone()),
                    sender_username: sender_username.clone(),
                    message_id: Some(msg.message_id.clone()),
                    chat_type: Some(msg.chat_type.clone()),
                    platform: Some(FEISHU_CHANNEL.to_string()),
                    im_attachments,
                    im_documents,
                };
                tokio::task::spawn_local(async move {
                    let chat_id = msg.chat_id.clone();

                    // ── Fetch quoted/reply parent text if present ──────────
                    let mut input_text = text.clone();
                    if let Some(parent_id) = &msg.parent_id {
                        match gateway.fetch_message_text(parent_id).await {
                            Ok(Some(parent_text)) => {
                                input_text =
                                    format!("[引用]\n{parent_text}\n\n[回复]\n{input_text}");
                            }
                            Ok(None) => {}
                            Err(e) => warn!("fetch parent message failed: {e:#}"),
                        }
                    }

                    // ── Build Content: text-only or multimodal ─────────────
                    let had_images = !msg.images.is_empty();
                    let mut image_data_urls: Vec<String> = Vec::new();
                    for image_key in &msg.images {
                        match gateway.download_image(&msg.message_id, image_key).await {
                            Ok((content_type, bytes)) => {
                                let b64 = base64::engine::general_purpose::STANDARD
                                    .encode(&bytes);
                                image_data_urls.push(format!("data:{content_type};base64,{b64}"));
                            }
                            Err(e) => warn!("download_image {image_key} failed: {e:#}"),
                        }
                    }
                    let content = build_message_content(
                        &input_text,
                        &image_data_urls,
                        had_images,
                        msg.files.len(),
                        msg.documents.len(),
                    );

                    // ── Add "thinking" reaction (best-effort) ──────────────
                    let reaction_id = gateway
                        .add_reaction(&msg.message_id, "THINKING")
                        .await
                        .map_err(|e| {
                            warn!("add_reaction failed: {e:#}");
                            e
                        })
                        .ok();

                    // ── Stream reply via interactive card (5-min timeout) ──
                    let mut card = gateway.begin_streaming_reply(&msg.message_id);
                    let mut stream = std::pin::pin!(bot.stream_with_options(&chat_id, content, stream_opts));
                    let mut timed_out = false;
                    {
                        let sleep = tokio::time::sleep(Duration::from_secs(300));
                        tokio::pin!(sleep);
                        loop {
                            tokio::select! {
                                ev = stream.next() => match ev {
                                    None => break,
                                    Some(CatEvent::Text(delta)) => {
                                        if let Err(e) = card.push(&delta).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::Thinking(content)) => {
                                        let block = format!("\n\n> 💭 *思考中…*\n> {}\n\n", content.replace('\n', "\n> "));
                                        if let Err(e) = card.push(&block).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::ToolCall { name, args }) => {
                                        let args_str = serde_json::to_string(&args).unwrap_or_default();
                                        let block = format!("\n\n🔧 **{}**\n```\n{}\n```\n", name, args_str);
                                        if let Err(e) = card.push(&block).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::ToolCallResult { name, result }) => {
                                        let preview = if result.len() > 300 {
                                            format!("{}…", &result[..300])
                                        } else {
                                            result.clone()
                                        };
                                        let block = format!("✅ **{}** → `{}`\n", name, preview.replace('`', "'"));
                                        if let Err(e) = card.push(&block).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::Stats { prompt_tokens, completion_tokens, elapsed_ms }) => {
                                        let secs = elapsed_ms / 1000;
                                        let ms = elapsed_ms % 1000;
                                        let stats = format!(
                                            "\n\n---\n📊 *tokens: {}↑ {}↓ | 耗时: {}.{:03}s*",
                                            prompt_tokens, completion_tokens, secs, ms
                                        );
                                        if let Err(e) = card.push(&stats).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::Error(e)) => {
                                        warn!("agent error: {e:#}");
                                        let msg = format!("\n\n❌ *[错误: {}]*", e);
                                        card.push(&msg).await.ok();
                                    }
                                    Some(_) => {}
                                },
                                _ = &mut sleep => {
                                    warn!(chat = %chat_id, "reply timed out after 300s");
                                    timed_out = true;
                                    break;
                                }
                            }
                        }
                    }
                    if timed_out {
                        card.push("\n\n⚠️ *[回复超时，已自动中断]*").await.ok();
                    }
                    card.finish().await.ok();

                    // ── Remove "thinking" reaction (best-effort) ───────────
                    if let Some(rid) = reaction_id {
                        if let Err(e) = gateway.delete_reaction(&msg.message_id, &rid).await {
                            warn!("delete_reaction failed: {e:#}");
                        }
                    }
                });
            }

            FeishuEvent::ReactionReceived(reaction) => {
                let reaction_uuid = user_store.resolve_or_create(FEISHU_CHANNEL, &reaction.sender_user_id);
                info!(
                    sender = %reaction.sender_user_id,
                    uuid   = %reaction_uuid,
                    emoji  = %reaction.emoji_type,
                    msg    = %reaction.message_id,
                    "reaction received",
                );

                if matcher.is_banned(&reaction_uuid) {
                    warn!(
                        "ignoring reaction from blacklisted user {} (uuid: {})",
                        reaction.sender_user_id,
                        reaction_uuid,
                    );
                    continue;
                }

                // ── Fetch the reacted-to message for context ───────────
                let msg_text = match gateway.fetch_message_text(&reaction.message_id).await {
                    Ok(Some(t)) => t,
                    Ok(None) => {
                        warn!(
                            "reacted message {} has no fetchable text",
                            reaction.message_id
                        );
                        continue;
                    }
                    Err(e) => {
                        warn!("fetch_message_text for reaction failed: {e:#}");
                        continue;
                    }
                };

                let thread_id = reaction.chat_id.clone();
                let input = format!(
                    "[用户对以下消息使用了「{}」表情]\n{}",
                    reaction.emoji_type, msg_text
                );

                // ── spawn_local: runs on same thread, no Send bound needed ─
                let bot = std::rc::Rc::clone(&bot);
                let gateway = gateway.clone();
                tokio::task::spawn_local(async move {
                    // ── Stream reply under the reacted message (5-min timeout) ──
                    let mut card = gateway.begin_streaming_reply(&reaction.message_id);
                    let mut stream = std::pin::pin!(bot.stream(&thread_id, input));
                    let mut timed_out = false;
                    {
                        let sleep = tokio::time::sleep(Duration::from_secs(300));
                        tokio::pin!(sleep);
                        loop {
                            tokio::select! {
                                ev = stream.next() => match ev {
                                    None => break,
                                    Some(CatEvent::Text(delta)) => {
                                        if let Err(e) = card.push(&delta).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::Thinking(content)) => {
                                        let block = format!("\n\n> 💭 *思考中…*\n> {}\n\n", content.replace('\n', "\n> "));
                                        if let Err(e) = card.push(&block).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::ToolCall { name, args }) => {
                                        let args_str = serde_json::to_string(&args).unwrap_or_default();
                                        let block = format!("\n\n🔧 **{}**\n```\n{}\n```\n", name, args_str);
                                        if let Err(e) = card.push(&block).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::ToolCallResult { name, result }) => {
                                        let preview = if result.len() > 300 {
                                            format!("{}…", &result[..300])
                                        } else {
                                            result.clone()
                                        };
                                        let block = format!("✅ **{}** → `{}`\n", name, preview.replace('`', "'"));
                                        if let Err(e) = card.push(&block).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::Stats { prompt_tokens, completion_tokens, elapsed_ms }) => {
                                        let secs = elapsed_ms / 1000;
                                        let ms = elapsed_ms % 1000;
                                        let stats = format!(
                                            "\n\n---\n📊 *tokens: {}↑ {}↓ | 耗时: {}.{:03}s*",
                                            prompt_tokens, completion_tokens, secs, ms
                                        );
                                        if let Err(e) = card.push(&stats).await {
                                            warn!("card push error: {e:#}");
                                        }
                                    }
                                    Some(CatEvent::Error(e)) => {
                                        warn!("agent error: {e:#}");
                                        let msg = format!("\n\n❌ *[错误: {}]*", e);
                                        card.push(&msg).await.ok();
                                    }
                                    Some(_) => {}
                                },
                                _ = &mut sleep => {
                                    warn!(thread = %thread_id, "reply timed out after 300s");
                                    timed_out = true;
                                    break;
                                }
                            }
                        }
                    }
                    if timed_out {
                        card.push("\n\n⚠️ *[回复超时，已自动中断]*").await.ok();
                    }
                    card.finish().await.ok();
                });
            }

            FeishuEvent::Unknown { event_type, .. } => {
                info!("ignored event type: {event_type}");
            }

            FeishuEvent::CardAction { .. } => {
                // Card button clicks — not used in standalone mode, ignore.
            }
        }
    }
    Ok::<(), anyhow::Error>(())
    }).await
}

async fn ensure_im_username(
    user_store: &UserStore,
    gateway: &FeishuGateway,
    user_uuid: &str,
    channel_user_id: &str,
) -> Option<String> {
    if let Some(username) = user_store.username(user_uuid) {
        return Some(username);
    }

    match gateway.get_user_name(channel_user_id).await {
        Ok(Some(username)) => {
            let username = username.trim().to_string();
            if username.is_empty() {
                return None;
            }
            if let Err(e) = user_store.set_username_if_missing(user_uuid, &username) {
                debug!(uuid = %user_uuid, sender = %channel_user_id, "failed to persist username: {e:#}");
            }
            user_store.username(user_uuid).or(Some(username))
        }
        Ok(None) => None,
        Err(e) => {
            debug!(sender = %channel_user_id, "failed to fetch Feishu username: {e:#}");
            None
        }
    }
}

fn build_message_content(
    text: &str,
    image_urls: &[String],
    had_images: bool,
    attachment_count: usize,
    document_count: usize,
) -> Content {
    let trimmed = text.trim();
    let valid_images: Vec<String> = image_urls
        .iter()
        .map(|url| url.trim())
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    if !valid_images.is_empty() {
        let mut parts: Vec<ContentPart> = Vec::new();
        if !trimmed.is_empty() {
            parts.push(ContentPart::text(trimmed.to_string()));
        }
        for data_url in valid_images {
            parts.push(ContentPart::image_url(data_url));
        }
        return Content::parts(parts);
    }

    if !trimmed.is_empty() {
        return Content::text(trimmed.to_string());
    }

    Content::text(fallback_message_text(
        had_images,
        attachment_count,
        document_count,
    ))
}

fn fallback_message_text(
    had_images: bool,
    attachment_count: usize,
    document_count: usize,
) -> String {
    match (had_images, attachment_count > 0, document_count > 0) {
        (true, _, _) => "[用户发送了图片]".to_string(),
        (false, true, true) => "[用户发送了附件和文档链接]".to_string(),
        (false, true, false) => "[用户发送了附件]".to_string(),
        (false, false, true) => "[用户发送了文档链接]".to_string(),
        (false, false, false) => "[用户发送了一条空白消息]".to_string(),
    }
}
