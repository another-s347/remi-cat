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

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bot_core::{
    todo::current_todo_card_markdown, CatBot, CatEvent, Content, ContentPart, ImAttachment,
    ImDocument, ImFileBridge, StreamOptions,
};
use futures::StreamExt;
use remi_proto::{
    agent_message::Payload as AgentPayload, AgentDone, AgentError as AgentErrorMsg, AgentMessage,
    AgentStats, AgentTextDelta, AgentThinking, AgentTodoState, AgentToolCall, AgentToolResult,
    DaemonPayload, DaemonServiceClient,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    // ── Sentry ────────────────────────────────────────────────────
    let _sentry_guard = sentry::init((
        std::env::var("SENTRY_DSN").unwrap_or_default(),
        sentry::ClientOptions {
            release: sentry::release_name!(),
            ..Default::default()
        },
    ));

    // ── Logging ───────────────────────────────────────────────────
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer().with_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "remi_cat_agent=info,bot_core=info".into()),
            ),
        )
        .with(sentry::integrations::tracing::layer())
        .init();

    info!("remi-cat starting up");

    // ── Config ────────────────────────────────────────────────────────────
    let daemon_addr =
        std::env::var("DAEMON_ADDR").unwrap_or_else(|_| "http://localhost:50051".into());

    // ── LocalSet: CatBot is !Send (uses Rc internally) ────────────────────
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let mut backoff = Duration::from_secs(1);
            const MAX_BACKOFF: Duration = Duration::from_secs(60);

            loop {
                info!(addr = %daemon_addr, "connecting to Daemon gRPC");
                match run_session(&daemon_addr).await {
                    Ok(()) => {
                        info!("Daemon session ended — reconnecting in 2 s");
                        // Always sleep before reconnecting to avoid a tight loop
                        // when the daemon closes the stream immediately (e.g. on
                        // restart or while another agent instance is being replaced).
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        backoff = Duration::from_secs(1);
                    }
                    Err(e) => {
                        error!("Daemon session error: {e:#} — reconnecting in {backoff:?}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                    }
                }
            }
        })
        .await;

    Ok(())
}

// ── Session ───────────────────────────────────────────────────────────────────

fn build_bot(bridge: Arc<dyn ImFileBridge>) -> Result<CatBot> {
    bot_core::CatBotBuilder::from_env()?
        .im_bridge(bridge)
        .build()
}

async fn run_session(addr: &str) -> Result<()> {
    let channel = Channel::from_shared(addr.to_string())?
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await?;

    let mut client = DaemonServiceClient::new(channel)
        .max_decoding_message_size(remi_proto::GRPC_MESSAGE_LIMIT_BYTES)
        .max_encoding_message_size(remi_proto::GRPC_MESSAGE_LIMIT_BYTES);

    // Track which secret keys were set in the last SecretsSync so we can
    // remove keys that have been deleted from the store.
    let mut last_secret_keys: HashSet<String> = HashSet::new();

    // Per-chat active task handles — used to cancel running LLM calls.
    // Each entry is (join_handle, cancel_notify) so cancellation is
    // cooperative: signalling the Notify lets stream_with_options
    // persist partial content and emit Done before the task exits.
    let active: Rc<
        RefCell<HashMap<String, (tokio::task::JoinHandle<()>, Arc<tokio::sync::Notify>)>>,
    > = Rc::new(RefCell::new(HashMap::new()));

    // Outgoing channel: per-message tasks → single gRPC outbound stream.
    let (out_tx, out_rx) = mpsc::channel::<AgentMessage>(512);
    let bridge_client = Arc::new(rpc_client::GrpcImFileBridge::new(out_tx.clone()));
    let bridge: Arc<dyn ImFileBridge> = bridge_client.clone();
    let mut bot: Option<Rc<CatBot>> = match build_bot(Arc::clone(&bridge)) {
        Ok(b) => {
            info!("CatBot ready (initialized from env)");
            Some(Rc::new(b))
        }
        Err(e) => {
            info!("CatBot not yet ready ({e}); will initialize after SecretsSync");
            None
        }
    };
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
            DaemonPayload::ImBridgeResponse(response) => {
                bridge_client.handle_response(response).await;
            }
            DaemonPayload::ImMessage(ev) => {
                info!(
                    message_id = %ev.message_id,
                    chat_id = %ev.chat_id,
                    sender_user_id = %ev.sender_user_id,
                    sender_username = %ev.sender_username,
                    has_sender_username = !ev.sender_username.trim().is_empty(),
                    "agent received IM message event"
                );
                let chat_id = ev.chat_id.clone();
                let reply_to = ev.message_id.clone();
                let trimmed = ev.text.trim().to_string();

                // If CatBot is not yet initialized (waiting for SecretsSync),
                // reply with a transient error instead of panicking.
                let Some(bot_rc) = bot.as_ref().map(Rc::clone) else {
                    let tx = out_tx.clone();
                    tokio::task::spawn_local(async move {
                        let _ = tx
                            .send(AgentMessage {
                                reply_to_message_id: reply_to.clone(),
                                payload: Some(AgentPayload::TextDelta(AgentTextDelta {
                                    text: "⚠️ Agent 正在初始化（等待凭证同步），请稍后重试。"
                                        .into(),
                                })),
                            })
                            .await;
                        let _ = tx
                            .send(AgentMessage {
                                reply_to_message_id: reply_to,
                                payload: Some(AgentPayload::Done(AgentDone {})),
                            })
                            .await;
                    });
                    continue;
                };
                // ── /cancel — cooperatively cancel the running task for this chat ──
                if trimmed == "/cancel" {
                    // Signal the active task (if any) to stop gracefully.  The
                    // daemon's queue worker has already preempted and fired the
                    // old completion channel, so the old task's eventual Done is
                    // a no-op; we still signal here for belt-and-suspenders.
                    if let Some((_, cancel)) = active.borrow_mut().remove(&chat_id) {
                        cancel.notify_one();
                    }
                    let tx = out_tx.clone();
                    tokio::task::spawn_local(async move {
                        let _ = tx
                            .send(AgentMessage {
                                reply_to_message_id: reply_to.clone(),
                                payload: Some(AgentPayload::TextDelta(AgentTextDelta {
                                    text: "✅ 已取消正在运行的任务（如有），已记录的内容将被保存。"
                                        .into(),
                                })),
                            })
                            .await;
                        let _ = tx
                            .send(AgentMessage {
                                reply_to_message_id: reply_to,
                                payload: Some(AgentPayload::Done(AgentDone {})),
                            })
                            .await;
                    });
                    continue;
                }

                // ── /tools — list registered tools ────────────────────────
                if trimmed == "/tools" {
                    let bot_t = Rc::clone(&bot_rc);
                    let tx = out_tx.clone();
                    tokio::task::spawn_local(async move {
                        let tools = bot_t.tool_list();
                        let mut text = "**可用工具列表：**\n\n".to_string();
                        for (name, desc) in &tools {
                            text.push_str(&format!("• `{name}` — {desc}\n"));
                        }
                        let _ = tx
                            .send(AgentMessage {
                                reply_to_message_id: reply_to.clone(),
                                payload: Some(AgentPayload::TextDelta(AgentTextDelta { text })),
                            })
                            .await;
                        let _ = tx
                            .send(AgentMessage {
                                reply_to_message_id: reply_to,
                                payload: Some(AgentPayload::Done(AgentDone {})),
                            })
                            .await;
                    });
                    continue;
                }

                // ── Regular message — spawn task and track the handle ─────
                let cancel = Arc::new(tokio::sync::Notify::new());
                // If there is already an in-flight task for this chat (shouldn't
                // normally happen because the daemon preempts via ChatCancelSignal
                // before sending a new message, but handle it defensively).
                if let Some((old_handle, old_cancel)) = active.borrow_mut().remove(&chat_id) {
                    old_cancel.notify_one();
                    // Allow the old task to drain without blocking; it will send
                    // Done for its own message_id which is now a daemon no-op.
                    drop(old_handle);
                }
                let bot_m = Rc::clone(&bot_rc);
                let tx = out_tx.clone();
                let active_m = Rc::clone(&active);
                let chat_id_m = chat_id.clone();
                let cancel_m = Arc::clone(&cancel);
                let handle = tokio::task::spawn_local(async move {
                    handle_message(ev, bot_m, tx, cancel_m).await;
                    active_m.borrow_mut().remove(&chat_id_m);
                });
                active.borrow_mut().insert(chat_id, (handle, cancel));
            }
            DaemonPayload::ImReaction(ev) => {
                if let Some(bot_rc) = bot.as_ref().map(Rc::clone) {
                    let tx = out_tx.clone();
                    tokio::task::spawn_local(async move {
                        handle_reaction(ev, bot_rc, tx).await;
                    });
                }
            }
            DaemonPayload::TriggerRun(ev) => {
                info!(
                    execution_id = %ev.execution_id,
                    trigger_uuid = %ev.trigger_uuid,
                    trigger_name = %ev.trigger_name,
                    chat_id = %ev.chat_id,
                    sender_user_id = %ev.sender_user_id,
                    "agent received trigger run event"
                );
                let Some(bot_rc) = bot.as_ref().map(Rc::clone) else {
                    let tx = out_tx.clone();
                    let reply_to = ev.execution_id.clone();
                    tokio::task::spawn_local(async move {
                        let _ = tx
                            .send(AgentMessage {
                                reply_to_message_id: reply_to.clone(),
                                payload: Some(AgentPayload::TextDelta(AgentTextDelta {
                                    text: "⚠️ Agent 正在初始化（等待凭证同步），本次 trigger 延后失败。"
                                        .into(),
                                })),
                            })
                            .await;
                        let _ = tx
                            .send(AgentMessage {
                                reply_to_message_id: reply_to,
                                payload: Some(AgentPayload::Done(AgentDone {})),
                            })
                            .await;
                    });
                    continue;
                };

                let cancel = Arc::new(tokio::sync::Notify::new());
                if let Some((old_handle, old_cancel)) = active.borrow_mut().remove(&ev.chat_id) {
                    old_cancel.notify_one();
                    drop(old_handle);
                }
                let chat_id = ev.chat_id.clone();
                let chat_id_for_task = chat_id.clone();
                let bot_m = Rc::clone(&bot_rc);
                let tx = out_tx.clone();
                let active_m = Rc::clone(&active);
                let cancel_m = Arc::clone(&cancel);
                let handle = tokio::task::spawn_local(async move {
                    handle_trigger_run(ev, bot_m, tx, cancel_m).await;
                    active_m.borrow_mut().remove(&chat_id_for_task);
                });
                active.borrow_mut().insert(chat_id, (handle, cancel));
            }
            DaemonPayload::ChatCancel(ev) => {
                // The daemon queue worker preempted an in-flight task for this
                // chat.  Signal it to stop gracefully (memory will be saved).
                if let Some((_, cancel)) = active.borrow_mut().remove(&ev.chat_id) {
                    cancel.notify_one();
                    info!(chat_id = %ev.chat_id, "received ChatCancelSignal — cooperative cancel signalled");
                }
            }
            DaemonPayload::Shutdown(_) => {
                info!("received Shutdown signal from Daemon — exiting");
                std::process::exit(0);
            }

            DaemonPayload::SecretsSync(sync) => {
                // Apply secrets as environment variables, removing any that
                // were present in the previous sync but are gone now.
                apply_secrets_sync(sync.entries.clone(), &mut last_secret_keys);
                match bot.as_ref() {
                    None => {
                        // First sync — try to initialize CatBot now that API credentials
                        // have been injected into the environment from the secret store.
                        match build_bot(Arc::clone(&bridge)) {
                            Ok(b) => {
                                info!("CatBot ready (initialized from SecretsSync)");
                                bot = Some(Rc::new(b));
                            }
                            Err(e) => {
                                warn!("CatBot init failed after SecretsSync: {e:#}");
                            }
                        }
                    }
                    Some(b) => {
                        // Rebuild the redactor so bash/fs tools scrub the new values.
                        b.update_secret_redactor(&sync.entries);
                    }
                }
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
    cancel: Arc<tokio::sync::Notify>,
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
        send_command_reply(&tx, &reply_to, text).await;
        return;
    }
    if trimmed == "/trigger" || trimmed.starts_with("/trigger ") {
        let text = match parse_trigger_slash_command(trimmed) {
            Ok(TriggerSlashCommand::Help) => trigger_command_help_text(),
            Ok(TriggerSlashCommand::List) => bot
                .trigger_list_command(&ev.chat_id, trigger_command_stream_options(&ev))
                .await
                .unwrap_or_else(|err| format!("❌ trigger list 失败: {err:#}")),
            Ok(TriggerSlashCommand::Delete { id }) => bot
                .trigger_delete_command(&ev.chat_id, id, trigger_command_stream_options(&ev))
                .await
                .unwrap_or_else(|err| format!("❌ trigger delete 失败: {err:#}")),
            Ok(TriggerSlashCommand::Upsert { arguments_json }) => bot
                .trigger_upsert_command(
                    &ev.chat_id,
                    &arguments_json,
                    trigger_command_stream_options(&ev),
                )
                .await
                .unwrap_or_else(|err| format!("❌ trigger upsert 失败: {err:#}")),
            Err(err) => format!("❌ {err}\n\n{}", trigger_command_help_text()),
        };
        send_command_reply(&tx, &reply_to, text).await;
        return;
    }
    // Unknown slash command — return error, do NOT invoke LLM.
    // Note: /cancel and /tools are handled upstream in run_session before
    // a task is even spawned, so they won't reach here.
    if trimmed.starts_with('/') {
        let cmd_name = trimmed
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or(trimmed.trim_start_matches('/'));
        let text = format!(
            "❌ 未知指令: `/{cmd_name}`\n\n**Agent 支持的指令：**\n\
             • `/compact` — 压缩短期记忆\n\
             • `/cancel` — 取消正在运行的任务\n\
             • `/tools` — 列出可用工具\n\
             • `/trigger` — 管理当前线程的触发器"
        );
        send_command_reply(&tx, &reply_to, text).await;
        return;
    }

    // Build text (incorporating quoted/parent context from daemon-enriched text).
    let content = build_message_content(
        &ev.text,
        &ev.images,
        !ev.images.is_empty(),
        ev.attachments.len(),
        ev.documents.len(),
    );

    let opts = stream_options_from_im_message(&ev, Some(cancel));
    info!(
        message_id = %ev.message_id,
        chat_id = %ev.chat_id,
        sender_user_id = %ev.sender_user_id,
        sender_username = %ev.sender_username,
        has_sender_username = !ev.sender_username.trim().is_empty(),
        todo_create_via_sdk = ev.todo_create_via_sdk,
        trigger_tools_enabled = ev.trigger_tools_enabled,
        "handle_message: built StreamOptions"
    );
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

async fn handle_trigger_run(
    ev: remi_proto::TriggerRunEvent,
    bot: Rc<CatBot>,
    tx: mpsc::Sender<AgentMessage>,
    cancel: Arc<tokio::sync::Notify>,
) {
    let reply_to = ev.execution_id.clone();
    let opts = StreamOptions {
        sender_user_id: Some(ev.sender_user_id.clone()).filter(|s| !s.is_empty()),
        sender_username: Some(ev.sender_username.clone()).filter(|s| !s.is_empty()),
        message_id: Some(ev.reply_to_message_id.clone()).filter(|s| !s.is_empty()),
        chat_type: Some(ev.chat_type.clone()).filter(|s| !s.is_empty()),
        platform: Some(ev.platform.clone()).filter(|s| !s.is_empty()),
        todo_create_via_sdk: ev.todo_create_via_sdk,
        trigger_tools_enabled: ev.trigger_tools_enabled,
        trigger_run: true,
        im_attachments: Vec::new(),
        im_documents: Vec::new(),
        cancel: Some(cancel),
    };
    let stream = bot.stream_with_options(&ev.chat_id, Content::text(ev.request), opts);
    forward_stream(reply_to, stream, tx).await;
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TriggerSlashCommand {
    Help,
    List,
    Delete { id: u64 },
    Upsert { arguments_json: String },
}

fn parse_trigger_slash_command(text: &str) -> Result<TriggerSlashCommand, String> {
    let trimmed = text.trim();
    if !(trimmed == "/trigger" || trimmed.starts_with("/trigger ")) {
        return Err("not a /trigger command".to_string());
    }

    let rest = trimmed["/trigger".len()..].trim();
    if rest.is_empty() || rest.eq_ignore_ascii_case("help") {
        return Ok(TriggerSlashCommand::Help);
    }

    if rest.eq_ignore_ascii_case("list") || rest.eq_ignore_ascii_case("ls") {
        return Ok(TriggerSlashCommand::List);
    }

    if let Some(args) = rest
        .strip_prefix("delete")
        .or_else(|| rest.strip_prefix("del"))
        .or_else(|| rest.strip_prefix("rm"))
    {
        let id = args
            .trim()
            .parse::<u64>()
            .map_err(|_| "`/trigger delete` 需要一个数字 id。".to_string())?;
        return Ok(TriggerSlashCommand::Delete { id });
    }

    if let Some(arguments_json) = rest.strip_prefix("upsert") {
        let arguments_json = strip_optional_json_fence(arguments_json);
        if arguments_json.is_empty() {
            return Err("`/trigger upsert` 后面需要一个 JSON 对象。".to_string());
        }
        return Ok(TriggerSlashCommand::Upsert { arguments_json });
    }

    Err(format!("未知 trigger 子命令: `{rest}`"))
}

fn strip_optional_json_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    for prefix in ["```json", "```"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let rest = rest.trim();
            if let Some(stripped) = rest.strip_suffix("```") {
                return stripped.trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

fn trigger_command_help_text() -> String {
    "**Trigger 指令**\n\n\
• `/trigger` — 显示这份帮助\n\
• `/trigger list` — 列出当前线程的触发器\n\
• `/trigger delete <id>` — 删除一个触发器\n\
• `/trigger upsert <json>` — 创建或更新一个触发器\n\n\
`/trigger upsert` 的 JSON 需要和 `trigger__upsert` 一致，例如：\n\
```json\n\
{\n\
  \"name\": \"Morning summary\",\n\
  \"request\": \"Send me a concise work summary for today.\",\n\
  \"precondition\": [\n\
    {\n\
      \"rule\": \"cron('0 9 * * *')\",\n\
      \"description\": \"Every day at 09:00\"\n\
    }\n\
  ],\n\
  \"condition\": []\n\
}\n\
```"
        .to_string()
}

fn trigger_command_stream_options(ev: &remi_proto::ImMessageEvent) -> StreamOptions {
    StreamOptions {
        sender_user_id: Some(ev.sender_user_id.clone()).filter(|s| !s.is_empty()),
        sender_username: Some(ev.sender_username.clone()).filter(|s| !s.is_empty()),
        message_id: Some(ev.message_id.clone()).filter(|s| !s.is_empty()),
        chat_type: Some(ev.chat_type.clone()).filter(|s| !s.is_empty()),
        platform: Some(ev.platform.clone()).filter(|s| !s.is_empty()),
        todo_create_via_sdk: ev.todo_create_via_sdk,
        trigger_tools_enabled: ev.trigger_tools_enabled,
        trigger_run: false,
        im_attachments: Vec::new(),
        im_documents: Vec::new(),
        cancel: None,
    }
}

fn stream_options_from_im_message(
    ev: &remi_proto::ImMessageEvent,
    cancel: Option<Arc<tokio::sync::Notify>>,
) -> StreamOptions {
    StreamOptions {
        sender_user_id: Some(ev.sender_user_id.clone()).filter(|s| !s.is_empty()),
        sender_username: Some(ev.sender_username.clone()).filter(|s| !s.is_empty()),
        message_id: Some(ev.message_id.clone()).filter(|s| !s.is_empty()),
        chat_type: Some(ev.chat_type.clone()).filter(|s| !s.is_empty()),
        platform: Some(ev.platform.clone()).filter(|s| !s.is_empty()),
        todo_create_via_sdk: ev.todo_create_via_sdk,
        trigger_tools_enabled: ev.trigger_tools_enabled,
        trigger_run: false,
        im_attachments: ev
            .attachments
            .iter()
            .map(|attachment| ImAttachment {
                key: attachment.key.clone(),
                name: attachment.name.clone(),
                mime_type: attachment.mime_type.clone(),
                size_bytes: attachment.size_bytes,
                file_type: attachment.file_type.clone(),
            })
            .collect(),
        im_documents: ev
            .documents
            .iter()
            .map(|document| ImDocument {
                url: document.url.clone(),
                title: document.title.clone(),
                doc_type: document.doc_type.clone(),
                token: document.token.clone(),
            })
            .collect(),
        cancel,
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

async fn send_command_reply(
    tx: &mpsc::Sender<AgentMessage>,
    reply_to: &str,
    text: String,
) {
    let _ = tx
        .send(AgentMessage {
            reply_to_message_id: reply_to.to_string(),
            payload: Some(AgentPayload::TextDelta(AgentTextDelta { text })),
        })
        .await;
    let _ = tx
        .send(AgentMessage {
            reply_to_message_id: reply_to.to_string(),
            payload: Some(AgentPayload::Done(AgentDone {})),
        })
        .await;
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
                    CatEvent::StateUpdate(us) =>
                        Some(AgentPayload::TodoState(AgentTodoState {
                            markdown: current_todo_card_markdown(&us).unwrap_or_default(),
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
        })
        .await
        .ok();
    }

    tx.send(AgentMessage {
        reply_to_message_id: reply_to,
        payload: Some(AgentPayload::Done(AgentDone {})),
    })
    .await
    .ok();
}

#[cfg(test)]
mod tests {
    use super::{parse_trigger_slash_command, strip_optional_json_fence, TriggerSlashCommand};

    #[test]
    fn parses_trigger_list_command() {
        assert_eq!(
            parse_trigger_slash_command("/trigger list").expect("list command should parse"),
            TriggerSlashCommand::List
        );
    }

    #[test]
    fn parses_trigger_delete_command() {
        assert_eq!(
            parse_trigger_slash_command("/trigger delete 7")
                .expect("delete command should parse"),
            TriggerSlashCommand::Delete { id: 7 }
        );
    }

    #[test]
    fn parses_trigger_upsert_command_with_code_fence() {
        assert_eq!(
            parse_trigger_slash_command("/trigger upsert ```json\n{\"name\":\"n\"}\n```")
                .expect("upsert command should parse"),
            TriggerSlashCommand::Upsert {
                arguments_json: "{\"name\":\"n\"}".to_string(),
            }
        );
    }

    #[test]
    fn strips_optional_json_fence() {
        assert_eq!(
            strip_optional_json_fence("```json\n{\"ok\":true}\n```"),
            "{\"ok\":true}"
        );
    }
}
