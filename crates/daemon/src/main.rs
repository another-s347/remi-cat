//! `remi-daemon` — IM gateway daemon (runs outside Docker).
//!
//! Responsibilities:
//! - Connects to Feishu via WebSocket long connection.
//! - Parses `/commands`, handles system commands locally (Docker container
//!   lifecycle + self-update), forwards ordinary messages to the Agent via gRPC.
//! - Manages a single Agent connection via the `DaemonService` gRPC server.
//!
//! # CLI flags / subcommands
//!
//! | Subcommand / Flag                      | Description                                                  |
//! |----------------------------------------|--------------------------------------------------------------|
//! | *(none)*                               | Start the daemon normally.                                   |
//! | `--local`                              | Local mode: skip Docker entirely.                            |
//! | `secrets set <KEY> <VALUE>`            | Write a secret to the encrypted store and exit.              |
//! | `secrets delete <KEY>`                 | Remove a secret from the encrypted store and exit.           |
//! | `secrets list`                         | Print all stored secret keys (not values) and exit.          |
//! | `init-env`                             | Import known keys from `.env` / env vars into the secret    |
//! |                                        | store, strip those lines from `.env`, then exit.             |
//! | `init-env --dev`                       | Same, but **keep** `.env` intact (useful during development).|
//! |                                        |                                                              |
//!
//! # Configuration (environment variables)
//!
//! `FEISHU_APP_ID` and `FEISHU_APP_SECRET` are read from the env var first,
//! then from the encrypted secret store if the env var is absent.
//!
//! | Variable               | Required | Description                                      |
//! |------------------------|----------|--------------------------------------------------|
//! | `FEISHU_APP_ID`        | Yes*     | Feishu enterprise app ID                         |
//! | `FEISHU_APP_SECRET`    | Yes*     | Feishu enterprise app secret                     |
//! | `DAEMON_GRPC_ADDR`     | No       | gRPC listen address (default: `0.0.0.0:50051`)   |
//! | `DAEMON_MGMT_ADDR`     | No       | Management WS address (default: `0.0.0.0:50052`) |
//! | `AGENT_CONTAINER`      | No       | Docker container name (default: `remi-cat`)      |
//! | `AGENT_BIN`            | No       | Path to `remi-cat-agent` binary (local mode only)|
//! | `DAEMON_UPDATE_URL`    | No       | URL to download new daemon binary for `/update`  |
//! | `GITHUB_REPO`          | No       | `owner/repo` for auto-constructing update URL    |
//! | `REMI_CAT_OWNER_ID`    | No       | Pre-configure owner (skip `/pair`)               |
//! | `RUST_LOG`             | No       | Log filter (default: `remi_daemon=info`)         |
//!
//! \* Can also be stored in the encrypted secret store (see `secret_store.rs`).

mod command;
mod docker;
mod local_agent;
mod mgmt_server;
mod restart;
mod rpc_server;
mod secret_store;

use anyhow::{Context, Result};
use base64::Engine as _;
use im_feishu::{FeishuEvent, FeishuGateway, FeishuMessage};
use matcher::{OwnerMatcher, OwnerStatus, PAIR_COMMAND};
use mgmt_server::MgmtContext;
use restart::{signal_ready, RestartHandle};
use rpc_server::{daemon_msg_im_message, daemon_msg_im_reaction, RpcServer};
use secret_store::SecretStore;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex, RwLock};
use tonic::transport::Server;
use tracing::{debug, error, info, warn};

/// Maximum number of messages that can be queued per chat before new messages
/// are rejected with a backpressure reply.
const CHAT_QUEUE_CAPACITY: usize = 5;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present (silently ignored when absent)
    let _ = dotenvy::dotenv();

    // ── CLI dispatch ──────────────────────────────────────────────────────
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        // secrets set KEY VALUE
        [cmd, sub, key, value] if cmd == "secrets" && sub == "set" => {
            let mut store = SecretStore::load(SecretStore::resolve_path())?;
            store.set(key, value)?;
            println!(
                "✅ secret `{key}` saved to {}",
                SecretStore::resolve_path().display()
            );
            return Ok(());
        }
        // secrets delete KEY
        [cmd, sub, key] if cmd == "secrets" && sub == "delete" => {
            let mut store = SecretStore::load(SecretStore::resolve_path())?;
            store.delete(key)?;
            println!("🗑️  secret `{key}` removed");
            return Ok(());
        }
        // secrets list
        [cmd, sub] if cmd == "secrets" && sub == "list" => {
            let store = SecretStore::load(SecretStore::resolve_path())?;
            let keys = store.keys();
            if keys.is_empty() {
                println!("(secret store is empty)");
            } else {
                println!("stored secret keys ({}):", keys.len());
                for k in keys {
                    println!("  {k}");
                }
            }
            return Ok(());
        }
        // init-env [--dev] — import .env / env vars into the store
        [cmd] if cmd == "init-env" => {
            return cmd_init_env(false);
        }
        [cmd, flag] if cmd == "init-env" && flag == "--dev" => {
            return cmd_init_env(true);
        }
        _ => {}
    }

    // ── CLI flags ─────────────────────────────────────────────────────────
    let local_mode = std::env::args().any(|a| a == "--local");

    // ── Logging ───────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "remi_daemon=info,im_feishu=info".into()),
        )
        .init();

    // ── Config ────────────────────────────────────────────────────────────
    // Load the secret store first so credentials stored there can be used
    // as a fallback when the corresponding env vars are not set.
    let secret_store_inner = SecretStore::load(SecretStore::resolve_path())?;

    let app_id = config_or_secret("FEISHU_APP_ID", &secret_store_inner)
        .ok_or_else(|| anyhow::anyhow!("FEISHU_APP_ID must be set (env var or secret store)"))?;
    let app_secret =
        config_or_secret("FEISHU_APP_SECRET", &secret_store_inner).ok_or_else(|| {
            anyhow::anyhow!("FEISHU_APP_SECRET must be set (env var or secret store)")
        })?;

    let secret_store = Arc::new(RwLock::new(secret_store_inner));

    let grpc_addr: SocketAddr = std::env::var("DAEMON_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50051".into())
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid DAEMON_GRPC_ADDR: {e}"))?;

    let agent_container = std::env::var("AGENT_CONTAINER").unwrap_or_else(|_| "remi-cat".into());

    // ── Components ────────────────────────────────────────────────────────
    let data_dir = std::path::PathBuf::from(
        std::env::var("REMI_DATA_DIR").unwrap_or_else(|_| ".remi-cat".into()),
    );

    let gateway = FeishuGateway::new(&app_id, &app_secret);
    let matcher = OwnerMatcher::load();
    let docker: Option<docker::DockerManager> = if local_mode {
        info!("--local flag set — running without Docker");
        None
    } else {
        let d = docker::DockerManager::new(&agent_container).map_err(|e| {
            anyhow::anyhow!(
                "failed to connect to Docker daemon: {e:#}\n  (pass --local to run without Docker)"
            )
        })?;
        info!(container = %agent_container, "connected to Docker daemon");
        Some(d)
    };
    let restart = RestartHandle::new();
    let start_time = Instant::now();
    let (rpc, _agent_sink) = RpcServer::new(gateway.clone(), Arc::clone(&secret_store));

    // Per-chat serial queues: one bounded mpsc channel per chat_id.
    let chat_queues: Arc<Mutex<HashMap<String, mpsc::Sender<FeishuMessage>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    info!("remi-daemon starting up");
    if let Some(id) = matcher.owner_id() {
        info!("owner: {id}");
    } else {
        info!("no owner set — send '{PAIR_COMMAND}' to claim ownership");
    }

    // ── Start gRPC server ─────────────────────────────────────────────────
    let rpc_clone = rpc.clone();
    let grpc_server = Server::builder().add_service(rpc_clone.into_server());

    // Bind the listener so we can signal readiness before serving.
    let listener = tokio::net::TcpListener::bind(grpc_addr)
        .await
        .map_err(|e| anyhow::anyhow!("failed to bind gRPC addr {grpc_addr}: {e}"))?;

    info!(addr = %grpc_addr, "gRPC server listening");

    // ── Start management server ───────────────────────────────────────────
    let mgmt_addr: SocketAddr = std::env::var("DAEMON_MGMT_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50052".into())
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid DAEMON_MGMT_ADDR: {e}"))?;

    let mgmt_ctx = MgmtContext {
        docker: docker.clone(),
        owner: matcher.clone(),
        secret_store: Arc::clone(&secret_store),
        rpc: rpc.clone(),
        data_dir: data_dir.clone(),
        start_time,
        sys: Arc::new(std::sync::Mutex::new(sysinfo::System::new())),
    };
    let mgmt = mgmt_server::MgmtServer::new(mgmt_ctx)?;
    info!("mgmt token: {}", mgmt.token().await);
    info!("mgmt fingerprint: {}", mgmt.fingerprint());
    tokio::spawn(async move {
        if let Err(e) = mgmt.serve(mgmt_addr).await {
            error!("mgmt server error: {e:#}");
        }
    });

    // ── Signal readiness to parent (if this is a restart child) ──────────
    signal_ready();

    // ── Auto-start agent in local mode ───────────────────────────────────
    if local_mode {
        // Replace 0.0.0.0 with 127.0.0.1 so the agent connects to localhost.
        let connect_addr = format!(
            "http://{}",
            grpc_addr.to_string().replace("0.0.0.0", "127.0.0.1")
        );
        let supervisor = local_agent::LocalAgentSupervisor::new(connect_addr)?;
        tokio::spawn(supervisor.supervise());
    }

    // Spawn gRPC serve task.
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    tokio::spawn(async move {
        if let Err(e) = grpc_server.serve_with_incoming(incoming).await {
            error!("gRPC server error: {e:#}");
        }
    });

    // ── Start Feishu gateway ──────────────────────────────────────────────
    let mut rx = gateway.start().await?;

    info!("remi-daemon ready");

    // ── Main event loop ───────────────────────────────────────────────────
    while let Some(event) = rx.recv().await {
        match event {
            FeishuEvent::MessageReceived(msg) => {
                let text = msg.text.trim().to_string();

                // ── Group chat: only handle @-mentions ────────────────────
                if msg.chat_type == "group" && !msg.at_bot {
                    debug!(
                        sender = %msg.sender_open_id,
                        chat   = %msg.chat_id,
                        "group message without @bot — skipping",
                    );
                    continue;
                }

                info!(
                    sender = %msg.sender_open_id,
                    chat   = %msg.chat_id,
                    "received: {text}",
                );

                // ── Pairing command ────────────────────────────────────────
                if text.trim() == PAIR_COMMAND {
                    let reply = match matcher.check(&msg.sender_open_id) {
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
                    send_reply(&gateway, &msg.message_id, &msg.chat_id, &reply).await;
                    continue;
                }

                // ── Command detection ──────────────────────────────────────
                if let Some(cmd) = im_gateway::ImCommand::parse(
                    &msg.message_id,
                    &msg.sender_open_id,
                    &msg.chat_id,
                    &text,
                ) {
                    if command::is_daemon_command(&cmd.name) {
                        // Daemon commands: owner-only AND private chat (单聊) only.
                        if msg.chat_type != "p2p" {
                            send_reply(
                                &gateway,
                                &msg.message_id,
                                &msg.chat_id,
                                "⚠️ 系统指令只能在单聊中执行。",
                            )
                            .await;
                            continue;
                        }
                        if !is_owner(&matcher, &msg.sender_open_id) {
                            send_reply(
                                &gateway,
                                &msg.message_id,
                                &msg.chat_id,
                                "⚠️ 仅主人可以执行系统指令。",
                            )
                            .await;
                            continue;
                        }
                        let docker = docker.clone();
                        let restart = restart.clone();
                        let gateway = gateway.clone();
                        let rpc = rpc.clone();
                        let secret_store = Arc::clone(&secret_store);
                        let msg_id = msg.message_id.clone();
                        let chat_id = msg.chat_id.clone();
                        tokio::spawn(async move {
                            let reply = command::execute_daemon_command(
                                &cmd.name,
                                &cmd.args,
                                docker.as_ref(),
                                &restart,
                                &secret_store,
                                &rpc,
                                &gateway,
                                &msg_id,
                            )
                            .await
                            .unwrap_or_else(|e| format!("❌ 命令失败: {e:#}"));
                            if !reply.is_empty() {
                                send_reply(&gateway, &msg_id, &chat_id, &reply).await;
                            }
                        });
                        continue;
                    }
                    // Non-daemon commands: no owner/chat restriction — forward to Agent.
                } else {
                    // Regular (non-command) message: owner-only.
                    if !is_owner(&matcher, &msg.sender_open_id) {
                        warn!("ignoring message from non-owner {}", msg.sender_open_id);
                        continue;
                    }
                }

                // ── Forward to Agent ───────────────────────────────────────
                let chat_id = msg.chat_id.clone();
                let msg_id = msg.message_id.clone();
                let queue_tx = {
                    let mut queues = chat_queues.lock().await;
                    // Respawn worker if the channel was closed.
                    if queues
                        .get(&chat_id)
                        .map(|tx| tx.is_closed())
                        .unwrap_or(true)
                    {
                        let tx = spawn_queue_worker(chat_id.clone(), gateway.clone(), rpc.clone());
                        queues.insert(chat_id.clone(), tx);
                    }
                    queues[&chat_id].clone()
                };
                match queue_tx.try_send(msg) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        warn!(chat = %chat_id, msg = %msg_id, "chat queue full");
                        send_reply(
                            &gateway,
                            &msg_id,
                            &chat_id,
                            &format!(
                                "⚠️ 消息队列已满（最多 {CHAT_QUEUE_CAPACITY} 条待处理），请等当前消息处理完成后再发送。"
                            ),
                        )
                        .await;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        warn!(chat = %chat_id, msg = %msg_id, "chat queue worker died");
                        chat_queues.lock().await.remove(&chat_id);
                        send_reply(&gateway, &msg_id, &chat_id, "⚠️ 内部错误，请稍后重试。").await;
                    }
                }
            }

            FeishuEvent::ReactionReceived(reaction) => {
                info!(
                    sender = %reaction.sender_open_id,
                    emoji  = %reaction.emoji_type,
                    msg    = %reaction.message_id,
                    "reaction received",
                );

                if !is_owner(&matcher, &reaction.sender_open_id) {
                    warn!(
                        "ignoring reaction from non-owner {}",
                        reaction.sender_open_id
                    );
                    continue;
                }

                let daemon_msg = daemon_msg_im_reaction(&reaction);
                if !rpc.send_to_agent(daemon_msg).await {
                    // Reaction with no agent — silently drop.
                }
            }

            FeishuEvent::CardAction {
                card_message_id,
                action_value,
                user_open_id,
            } => {
                info!(user = %user_open_id, card = %card_message_id, "card action received");

                if !is_owner(&matcher, &user_open_id) {
                    warn!("ignoring card action from non-owner {user_open_id}");
                    continue;
                }

                let gateway = gateway.clone();
                let rpc = rpc.clone();
                let secret_store = Arc::clone(&secret_store);
                tokio::spawn(async move {
                    let action = action_value
                        .get("action")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let result: anyhow::Result<()> = async {
                        match action.as_str() {
                            "delete" => {
                                let key = action_value["key"].as_str().ok_or_else(|| {
                                    anyhow::anyhow!("missing key in delete action")
                                })?;
                                secret_store.write().await.delete(key)?;
                                info!(key, "secret deleted via card");
                                rpc.sync_secrets_to_agent().await;
                            }
                            "set" => {
                                // form_value is nested under action.form_value
                                let fv = &action_value["form_value"];
                                let key = fv["new_key"]
                                    .as_str()
                                    .or_else(|| action_value["new_key"].as_str())
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("missing new_key in set action")
                                    })?;
                                let val = fv["new_value"]
                                    .as_str()
                                    .or_else(|| action_value["new_value"].as_str())
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("missing new_value in set action")
                                    })?;
                                secret_store.write().await.set(key, val)?;
                                info!(key, "secret set via card");
                                rpc.sync_secrets_to_agent().await;
                            }
                            "mark_system" => {
                                let key = action_value["key"].as_str().ok_or_else(|| {
                                    anyhow::anyhow!("missing key in mark_system action")
                                })?;
                                secret_store.write().await.mark_system(key)?;
                                info!(key, "secret marked as system via card");
                                rpc.sync_secrets_to_agent().await;
                            }
                            "unmark_system" => {
                                let key = action_value["key"].as_str().ok_or_else(|| {
                                    anyhow::anyhow!("missing key in unmark_system action")
                                })?;
                                secret_store.write().await.unmark_system(key)?;
                                info!(key, "secret unmarked as system via card");
                                rpc.sync_secrets_to_agent().await;
                            }
                            other => {
                                warn!("unknown card action: {other}");
                                return Ok(());
                            }
                        }
                        // Close the card after a successful action.
                        if let Err(e) = gateway.delete_message(&card_message_id).await {
                            warn!("delete card message after action failed: {e:#}");
                        }
                        Ok(())
                    }
                    .await;

                    if let Err(e) = result {
                        warn!("card action handler error: {e:#}");
                    }
                });
            }

            FeishuEvent::Unknown { event_type, .. } => {
                info!("ignored event type: {event_type}");
            }
        }
    }

    Ok(())
}

// ── Chat queue worker ─────────────────────────────────────────────────────────

/// Spawn a serial queue worker for `chat_id`.
///
/// The worker blocks on one message at a time (enrich → react → agent →
/// await completion) so messages from the same chat are processed in order.
fn spawn_queue_worker(
    chat_id: String,
    gateway: FeishuGateway,
    rpc: RpcServer,
) -> mpsc::Sender<FeishuMessage> {
    let (tx, mut rx) = mpsc::channel::<FeishuMessage>(CHAT_QUEUE_CAPACITY);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let message_id = msg.message_id.clone();
            let enriched = enrich_message(&gateway, msg).await;

            let reaction_id = gateway
                .add_reaction(&enriched.message_id, "THINKING")
                .await
                .map_err(|e| warn!("add_reaction failed: {e:#}"))
                .ok();

            let daemon_msg = daemon_msg_im_message(&enriched);
            if let Some(completion_rx) = rpc.send_to_agent_and_await(daemon_msg, &message_id).await
            {
                if let Some(rid) = reaction_id {
                    rpc.record_reaction(&message_id, rid).await;
                }
                // Block until the Agent signals Done/Error for this message.
                completion_rx.await.ok();
            } else {
                // Agent not connected.
                if let Some(rid) = reaction_id {
                    gateway.delete_reaction(&message_id, &rid).await.ok();
                }
                rpc.reply_agent_unavailable(&message_id).await;
            }
        }
        info!(chat = %chat_id, "chat queue worker exiting");
    });
    tx
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Read a configuration value from an env var first, then fall back to the
/// secret store.  Returns `None` if neither source has the key.
pub fn config_or_secret(env_var: &str, store: &secret_store::SecretStore) -> Option<String> {
    std::env::var(env_var)
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| store.get(env_var).map(str::to_string))
}

fn is_owner(matcher: &OwnerMatcher, sender_id: &str) -> bool {
    matches!(matcher.check(sender_id), OwnerStatus::Owner)
}

async fn send_reply(gateway: &FeishuGateway, message_id: &str, chat_id: &str, text: &str) {
    if let Err(e) = gateway.reply_text(message_id, text).await {
        warn!("reply_text failed: {e:#}");
        gateway.send_text(chat_id, text).await.ok();
    }
}

/// Fetch quoted/parent text and download images, returning an enriched message.
/// Image keys are replaced with base64 data URLs so the Agent doesn't need
/// Feishu credentials.
async fn enrich_message(gateway: &FeishuGateway, mut msg: FeishuMessage) -> FeishuMessage {
    // ── Parent message text ─────────────────────────────────────────────
    if let Some(parent_id) = &msg.parent_id {
        match gateway.fetch_message_text(parent_id).await {
            Ok(Some(parent_text)) => {
                msg.text = format!("[引用]\n{parent_text}\n\n[回复]\n{}", msg.text);
            }
            Ok(None) => {}
            Err(e) => warn!("fetch parent message failed: {e:#}"),
        }
    }

    // ── Images → base64 data URLs ───────────────────────────────────────
    let mut data_urls: Vec<String> = Vec::new();
    for image_key in &msg.images {
        match gateway.download_image(&msg.message_id, image_key).await {
            Ok((content_type, bytes)) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                data_urls.push(format!("data:{content_type};base64,{b64}"));
            }
            Err(e) => warn!("download_image {image_key} failed: {e:#}"),
        }
    }
    msg.images = data_urls;

    msg
}

// ── init-env subcommand ───────────────────────────────────────────────────────

/// Keys that are moved from `.env` / environment into the secret store by
/// `remi-daemon init-env` **and** stored as system secrets (not forwarded to
/// the Agent).
const SYSTEM_KEYS: &[&str] = &["FEISHU_APP_ID", "FEISHU_APP_SECRET", "OPENAI_API_KEY"];

/// Keys that are moved from `.env` / environment into the secret store by
/// `remi-daemon init-env`.
const INIT_ENV_KEYS: &[&str] = &[
    "FEISHU_APP_ID",
    "FEISHU_APP_SECRET",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_MODEL",
    "EXA_API_KEY",
];

/// Import known credential keys from the environment (already populated by
/// `dotenvy::dotenv()` above) into the encrypted secret store.
///
/// When `dev` is `false` the imported lines are stripped from `.env` so they
/// are no longer stored in plaintext.  When `dev` is `true` `.env` is left
/// unchanged (convenient during local development).
///
/// Keys that are empty / absent are skipped silently.
/// Keys that already exist in the store are overwritten.
fn cmd_init_env(dev: bool) -> Result<()> {
    let store_path = SecretStore::resolve_path();
    let mut store = SecretStore::load(store_path.clone())?;
    let mut imported: Vec<&str> = Vec::new();

    for &key in INIT_ENV_KEYS {
        let val = std::env::var(key).unwrap_or_default();
        if val.is_empty() {
            continue;
        }
        if SYSTEM_KEYS.contains(&key) {
            store.set_system(key, &val)?;
        } else {
            store.set(key, &val)?;
        }
        imported.push(key);
    }

    if imported.is_empty() {
        println!("ℹ️  No credential keys found in environment / .env — nothing imported.");
        println!("    Expected keys: {}", INIT_ENV_KEYS.join(", "));
        return Ok(());
    }

    println!(
        "✅ Imported {} key(s) into {}",
        imported.len(),
        store_path.display()
    );
    for k in &imported {
        println!("   • {k}");
    }

    if dev {
        println!("ℹ️  --dev mode: .env left unchanged.");
    } else {
        // Strip the imported keys from .env so they are no longer stored as plaintext.
        let dot_env_path = std::path::Path::new(".env");
        if dot_env_path.exists() {
            strip_keys_from_dotenv(dot_env_path, &imported)?;
            println!("🧹 Removed imported keys from .env");
        }
    }

    println!("\nYou can now start the daemon without any credentials in .env:");
    println!("  cargo run -p remi-daemon -- --local");
    Ok(())
}

/// Remove lines that set any of `keys` from a `.env` file in-place.
/// Lines are matched as `KEY=...` (with optional leading whitespace).
/// Comment lines and unrelated lines are preserved unchanged.
fn strip_keys_from_dotenv(path: &std::path::Path, keys: &[&str]) -> Result<()> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let filtered: String = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            // Keep comment lines and blank lines as-is.
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return true;
            }
            // Drop lines that assign one of the imported keys.
            if let Some(eq) = trimmed.find('=') {
                let lhs = trimmed[..eq].trim();
                if keys.contains(&lhs) {
                    return false;
                }
            }
            true
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Preserve a trailing newline if the original had one.
    let output = if content.ends_with('\n') {
        format!("{filtered}\n")
    } else {
        filtered
    };

    std::fs::write(path, output).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
