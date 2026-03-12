//! `remi-cat-agent` — LLM agent (runs inside Docker).
//!
//! Connects to `remi-daemon` via gRPC bidirectional streaming, receives Feishu
//! events, processes them with `CatBot`, and streams replies back.
//!
//! # Configuration (environment variables)
//!
//! | Variable           | Required | Description                                          |
//! |--------------------|----------|------------------------------------------------------|
//! | `DAEMON_ADDR`      | No       | Daemon gRPC address (default: `http://localhost:50051`) |
//! | `OPENAI_API_KEY`   | Yes      | OpenAI-compatible API key                            |
//! | `OPENAI_BASE_URL`  | No       | Custom base URL                                      |
//! | `OPENAI_MODEL`     | No       | Model name (default: `gpt-4o`)                       |
//! | `RUST_LOG`         | No       | Log filter (default: `remi_cat_agent=info`)          |

mod rpc_client;

use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use bot_core::{CatBot, CatEvent, Content, ContentPart, StreamOptions};
use futures::StreamExt;
use remi_proto::{
    AgentDone, AgentError as AgentErrorMsg, AgentMessage, AgentStats, AgentTextDelta,
    AgentThinking, AgentToolCall, AgentToolResult, DaemonPayload, DaemonServiceClient,
    agent_message::Payload as AgentPayload,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "remi_cat_agent=info,bot_core=info".into()),
        )
        .init();

    info!("remi-cat starting up");

    // ── Config ────────────────────────────────────────────────────────────
    let daemon_addr = std::env::var("DAEMON_ADDR")
        .unwrap_or_else(|_| "http://localhost:50051".into());

    // ── LocalSet: CatBot is !Send (uses Rc internally) ────────────────────
    let local = tokio::task::LocalSet::new();
    local.run_until(async move {
        let bot = Rc::new(CatBot::from_env().expect("CatBot init failed"));
        info!("CatBot ready");

        let mut backoff = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);

        loop {
            info!(addr = %daemon_addr, "connecting to Daemon gRPC");
            match run_session(&daemon_addr, Rc::clone(&bot)).await {
                Ok(()) => {
                    info!("Daemon session ended — reconnecting");
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    error!("Daemon session error: {e:#} — reconnecting in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }).await;

    Ok(())
}

// ── Session ───────────────────────────────────────────────────────────────────

async fn run_session(addr: &str, bot: Rc<CatBot>) -> Result<()> {
    let channel = Channel::from_shared(addr.to_string())?
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await?;

    let mut client = DaemonServiceClient::new(channel);

    // Track which secret keys were set in the last SecretsSync so we can
    // remove keys that have been deleted from the store.
    let mut last_secret_keys: HashSet<String> = HashSet::new();

    // Outgoing channel: per-message tasks → single gRPC outbound stream.
    let (out_tx, out_rx) = mpsc::channel::<AgentMessage>(512);
    let outbound = ReceiverStream::new(out_rx);

    let response = client.agent_connect(outbound).await?;
    let mut inbound: tonic::Streaming<remi_proto::DaemonMessage> = response.into_inner();
    info!("connected to Daemon");

    while let Some(result) = inbound.next().await {
        let daemon_msg = match result {
            Ok(m) => m,
            Err(e) => {
                warn!("gRPC stream error: {e}");
                return Err(anyhow::anyhow!("gRPC stream error: {e}"));
            }
        };

        let Some(payload) = daemon_msg.payload else {
            continue;
        };

        match payload {
            DaemonPayload::ImMessage(ev) => {
                let bot = Rc::clone(&bot);
                let tx  = out_tx.clone();
                tokio::task::spawn_local(async move {
                    handle_message(ev, bot, tx).await;
                });
            }
            DaemonPayload::ImReaction(ev) => {
                let bot = Rc::clone(&bot);
                let tx  = out_tx.clone();
                tokio::task::spawn_local(async move {
                    handle_reaction(ev, bot, tx).await;
                });
            }
            DaemonPayload::Shutdown(_) => {
                info!("received Shutdown signal from Daemon — exiting");
                std::process::exit(0);
            }

            DaemonPayload::SecretsSync(sync) => {
                // Apply secrets as environment variables, removing any that
                // were present in the previous sync but are gone now.
                apply_secrets_sync(sync.entries.clone(), &mut last_secret_keys);
                // Rebuild the redactor so bash/fs tools scrub the new values.
                bot.update_secret_redactor(&sync.entries);
            }
        }
    }

    Ok(())
}

// ── SecretsSync application ──────────────────────────────────────────

/// Atomically apply a new secrets payload to the process environment.
///
/// - Keys present in `new_entries` are set via `std::env::set_var`.
/// - Keys that were present in the *previous* sync but are absent now are
///   removed via `std::env::remove_var`.
/// - `known_keys` is updated to reflect the new set.
///
/// # Safety note
/// `std::env::set_var` / `remove_var` are technically `unsafe` in Rust
/// but are the only portable way to inject env vars into a running process.
/// This function is called from the gRPC session task before any threaded
/// tool execution begins, so there is no concurrent env access.
fn apply_secrets_sync(
    new_entries: std::collections::HashMap<String, String>,
    known_keys: &mut HashSet<String>,
) {
    for (k, v) in &new_entries {
        // SAFETY: single-threaded Tokio local_set context; no concurrent env reads.
        unsafe { std::env::set_var(k, v) };
    }
    // Remove keys that disappeared from the store.
    for old_key in known_keys.iter() {
        if !new_entries.contains_key(old_key) {
            // SAFETY: same as above.
            unsafe { std::env::remove_var(old_key) };
        }
    }
    // Update the bot's redactor with the new entries.
    // (CatBot is in a separate Rc so bot.update_secret_redactor is called separately
    //  when this fn returns — see caller.)
    *known_keys = new_entries.into_keys().collect();
    tracing::debug!(count = known_keys.len(), "secrets synced to env");
}

// ── Message handler ───────────────────────────────────────────────────────────

async fn handle_message(
    ev: remi_proto::ImMessageEvent,
    bot: Rc<CatBot>,
    tx: mpsc::Sender<AgentMessage>,
) {
    let reply_to = ev.message_id.clone();

    // ── Hardcoded slash commands (handled before LLM) ─────────────────────
    let trimmed = ev.text.trim();
    if trimmed == "/compact" {
        let text = match bot.compact_memory(&ev.chat_id).await {
            Ok(0) => "✅ 短期记忆为空，无需压缩。".to_string(),
            Ok(n) => format!("✅ 已将 {n} 条短期记忆压缩为中期记忆。"),
            Err(e) => format!("❌ 压缩失败: {e:#}"),
        };
        let _ = tx.send(AgentMessage {
            reply_to_message_id: reply_to.clone(),
            payload: Some(AgentPayload::TextDelta(AgentTextDelta { text })),
        }).await;
        let _ = tx.send(AgentMessage {
            reply_to_message_id: reply_to,
            payload: Some(AgentPayload::Done(AgentDone {})),
        }).await;
        return;
    }
    // Unknown slash command — return error, do NOT invoke LLM.
    if trimmed.starts_with('/') {
        let cmd_name = trimmed
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or(trimmed.trim_start_matches('/'));
        let text = format!("❌ 未知指令: /{cmd_name}");
        let _ = tx.send(AgentMessage {
            reply_to_message_id: reply_to.clone(),
            payload: Some(AgentPayload::TextDelta(AgentTextDelta { text })),
        }).await;
        let _ = tx.send(AgentMessage {
            reply_to_message_id: reply_to,
            payload: Some(AgentPayload::Done(AgentDone {})),
        }).await;
        return;
    }

    // Build text (incorporating quoted/parent context from daemon-enriched text).
    let content = if ev.images.is_empty() {
        Content::text(ev.text.clone())
    } else {
        let mut parts: Vec<ContentPart> = Vec::new();
        if !ev.text.is_empty() {
            parts.push(ContentPart::text(ev.text.clone()));
        }
        for data_url in &ev.images {
            parts.push(ContentPart::image_url(data_url.clone()));
        }
        Content::parts(parts)
    };

    let opts = StreamOptions {
        sender_open_id: Some(ev.sender_open_id.clone()).filter(|s| !s.is_empty()),
        message_id:     Some(ev.message_id.clone()).filter(|s| !s.is_empty()),
        chat_type:      Some(ev.chat_type.clone()).filter(|s| !s.is_empty()),
    };
    let stream = bot.stream_with_options(&ev.chat_id, content, opts);
    forward_stream(reply_to, stream, tx).await;
}

async fn handle_reaction(
    ev: remi_proto::ImReactionEvent,
    bot: Rc<CatBot>,
    tx: mpsc::Sender<AgentMessage>,
) {
    let reply_to = ev.message_id.clone();
    let text = format!("[用户对该消息使用了「{}」表情]", ev.emoji_type);
    let stream = bot.stream(&ev.chat_id, text);
    forward_stream(reply_to, stream, tx).await;
}

// ── CatEvent → AgentMessage forwarding ───────────────────────────────────────

async fn forward_stream(
    reply_to: String,
    stream: impl futures::Stream<Item = CatEvent>,
    tx: mpsc::Sender<AgentMessage>,
) {
    use std::pin::pin;
    let mut timed_out = false;
    let mut stream = pin!(stream);

    let sleep = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            ev = stream.next() => {
                let Some(ev) = ev else { break };
                let payload: Option<AgentPayload> = match ev {
                    CatEvent::Text(t) =>
                        Some(AgentPayload::TextDelta(AgentTextDelta { text: t })),
                    CatEvent::Thinking(c) =>
                        Some(AgentPayload::Thinking(AgentThinking { content: c })),
                    CatEvent::ToolCall { name, args } =>
                        Some(AgentPayload::ToolCall(AgentToolCall {
                            name,
                            args_json: serde_json::to_string(&args).unwrap_or_default(),
                        })),
                    CatEvent::ToolCallResult { name, result } =>
                        Some(AgentPayload::ToolResult(AgentToolResult { name, result })),
                    CatEvent::Stats { prompt_tokens, completion_tokens, elapsed_ms } =>
                        Some(AgentPayload::Stats(AgentStats {
                            prompt_tokens,
                            completion_tokens,
                            elapsed_ms,
                        })),
                    CatEvent::Error(e) => {
                        tx.send(AgentMessage {
                            reply_to_message_id: reply_to.clone(),
                            payload: Some(AgentPayload::Error(AgentErrorMsg {
                                message: e.to_string(),
                            })),
                        }).await.ok();
                        return;
                    }
                    CatEvent::Done => break,
                    _ => None,
                };
                if let Some(p) = payload {
                    if tx.send(AgentMessage {
                        reply_to_message_id: reply_to.clone(),
                        payload: Some(p),
                    }).await.is_err() {
                        return; // Daemon disconnected
                    }
                }
            }
            _ = &mut sleep => {
                warn!(reply_to = %reply_to, "agent timed out after 300s");
                timed_out = true;
                break;
            }
        }
    }

    if timed_out {
        tx.send(AgentMessage {
            reply_to_message_id: reply_to.clone(),
            payload: Some(AgentPayload::TextDelta(AgentTextDelta {
                text: "\n\n⚠️ *[回复超时，已自动中断]*".into(),
            })),
        }).await.ok();
    }

    tx.send(AgentMessage {
        reply_to_message_id: reply_to,
        payload: Some(AgentPayload::Done(AgentDone {})),
    }).await.ok();
}
