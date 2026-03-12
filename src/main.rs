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

use base64::Engine as _;
use bot_core::{CatBot, CatEvent, Content, ContentPart};
use futures::StreamExt;
use im_feishu::{FeishuEvent, FeishuGateway};
use matcher::{OwnerMatcher, OwnerStatus, PAIR_COMMAND};
use std::{rc::Rc, time::Duration};
use tracing::{error, info, warn};

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
    let app_id = std::env::var("FEISHU_APP_ID")
        .map_err(|_| anyhow::anyhow!("FEISHU_APP_ID must be set"))?;
    let app_secret = std::env::var("FEISHU_APP_SECRET")
        .map_err(|_| anyhow::anyhow!("FEISHU_APP_SECRET must be set"))?;

    // ── Components ────────────────────────────────────────────────────────
    let gateway = FeishuGateway::new(app_id, app_secret);
    let matcher = OwnerMatcher::load();
    let bot = Rc::new(CatBot::from_env()?);

    info!("remi-cat starting up");
    if let Some(id) = matcher.owner_id() {
        info!("owner: {id}");
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
                info!(
                    sender = %msg.sender_open_id,
                    chat   = %msg.chat_id,
                    "received: {text}",
                );

                // ── Pairing command ────────────────────────────────────
                if text == PAIR_COMMAND {
                    let status = matcher.check(&msg.sender_open_id);
                    let reply = match status {
                        OwnerStatus::NeedPairing => {
                            if matcher.try_pair(&msg.sender_open_id) {
                                format!(
                                    "配对成功！您已成为我的主人。\nYour ID: {}",
                                    msg.sender_open_id
                                )
                            } else {
                                "配对失败，请重试。".into()
                            }
                        }
                        OwnerStatus::Owner => "您已是我的主人。".into(),
                        OwnerStatus::NotOwner => "我已有主人 :)".into(),
                    };
                    if let Err(e) = gateway.reply_text(&msg.message_id, &reply).await {
                        warn!("reply_text failed: {e:#}");
                        gateway.send_text(&msg.chat_id, &reply).await.ok();
                    }
                    continue;
                }

                // ── Non-owner ─────────────────────────────────────────
                match matcher.check(&msg.sender_open_id) {
                    OwnerStatus::NeedPairing | OwnerStatus::NotOwner => {
                        warn!("ignoring message from non-owner {}", msg.sender_open_id);
                        continue;
                    }
                    OwnerStatus::Owner => {}
                }

                // ── spawn_local: runs on same thread, no Send bound needed ─
                let bot = std::rc::Rc::clone(&bot);
                let gateway = gateway.clone();
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
                    let content = if msg.images.is_empty() {
                        Content::text(input_text)
                    } else {
                        let mut parts: Vec<ContentPart> = Vec::new();
                        if !input_text.is_empty() {
                            parts.push(ContentPart::text(input_text));
                        }
                        for image_key in &msg.images {
                            match gateway.download_image(&msg.message_id, image_key).await {
                                Ok((content_type, bytes)) => {
                                    let b64 = base64::engine::general_purpose::STANDARD
                                        .encode(&bytes);
                                    let data_url = format!("data:{content_type};base64,{b64}");
                                    parts.push(ContentPart::image_url(data_url));
                                }
                                Err(e) => warn!("download_image {image_key} failed: {e:#}"),
                            }
                        }
                        Content::parts(parts)
                    };

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
                    let mut stream = std::pin::pin!(bot.stream_content(&chat_id, content));
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
                info!(
                    sender = %reaction.sender_open_id,
                    emoji  = %reaction.emoji_type,
                    msg    = %reaction.message_id,
                    "reaction received",
                );

                // ── Non-owner ─────────────────────────────────────────
                match matcher.check(&reaction.sender_open_id) {
                    OwnerStatus::NeedPairing | OwnerStatus::NotOwner => {
                        warn!(
                            "ignoring reaction from non-owner {}",
                            reaction.sender_open_id
                        );
                        continue;
                    }
                    OwnerStatus::Owner => {}
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

