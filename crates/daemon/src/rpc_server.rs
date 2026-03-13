//! gRPC service implementation: DaemonService.
//!
//! The Daemon runs the gRPC *server*; the Agent connects as client.
//!
//! # Session model
//!
//! At most one Agent can be connected at a time (single-owner bot).  When the
//! Agent calls `AgentConnect`:
//!
//! 1. A `tokio::sync::mpsc` channel is created and its `Sender` stored in
//!    `AgentSink`.  The Daemon posts `DaemonMessage`s to this sender whenever
//!    a Feishu event arrives.
//! 2. The server task reads `AgentMessage`s from the client stream, looks up
//!    (or creates) the appropriate `StreamingCard`, and feeds text deltas into
//!    it.
//! 3. When the stream ends (Agent disconnects or container restarts) the
//!    `AgentSink` is cleared so the Daemon can accept a fresh connection.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

use im_feishu::{FeishuGateway, StreamingCard};
use remi_proto::{
    AgentPayload, DaemonMessage, DaemonPayload, DaemonService, DaemonServiceServer, ImMessageEvent,
    ImReactionEvent, SecretsSync, ShutdownSignal,
};

use crate::secret_store::SecretStore;

/// Shared map: Feishu message_id → reaction_id, for cleaning up "thinking" emoji.
pub type ReactionMap = Arc<Mutex<HashMap<String, String>>>;

/// Shared map: Feishu message_id → oneshot sender, for signalling queue workers
/// when the Agent finishes processing a message.
pub type CompletionMap = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;

// ── AgentSink ─────────────────────────────────────────────────────────────────

/// Shared handle to the currently connected Agent's outbound message channel.
pub type AgentSink = Arc<Mutex<Option<mpsc::Sender<Result<DaemonMessage, Status>>>>>;

// ── RpcServer ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct RpcServer {
    gateway: FeishuGateway,
    /// Shared sender — `Some` when Agent is connected.
    agent_sink: AgentSink,
    /// reaction_id keyed by message_id, for bookend cleanup on Done/Error.
    reactions: ReactionMap,
    /// oneshot completion senders keyed by message_id, for queue backpressure.
    completions: CompletionMap,
    /// Shared secret store — read on connect and on manual sync.
    secret_store: Arc<RwLock<SecretStore>>,
}

impl RpcServer {
    pub fn new(
        gateway: FeishuGateway,
        secret_store: Arc<RwLock<SecretStore>>,
    ) -> (Self, AgentSink) {
        let sink: AgentSink = Arc::new(Mutex::new(None));
        (
            Self {
                gateway,
                agent_sink: sink.clone(),
                reactions: Arc::new(Mutex::new(HashMap::new())),
                completions: Arc::new(Mutex::new(HashMap::new())),
                secret_store,
            },
            sink,
        )
    }

    /// Push all current secrets to the connected Agent as a `SecretsSync` message.
    /// No-op when no Agent is connected.
    pub async fn sync_secrets_to_agent(&self) {
        let entries: std::collections::HashMap<String, String> =
            self.secret_store.read().await.agent_entries();
        let msg = DaemonMessage {
            payload: Some(remi_proto::daemon_message::Payload::SecretsSync(
                SecretsSync { entries },
            )),
        };
        self.send_to_agent(msg).await;
    }

    /// Store a reaction_id so it can be cleaned up when the Agent finishes.
    pub async fn record_reaction(&self, message_id: &str, reaction_id: String) {
        self.reactions
            .lock()
            .await
            .insert(message_id.to_string(), reaction_id);
    }

    /// Build the tonic server wrapper for this service.
    pub fn into_server(self) -> DaemonServiceServer<Self> {
        DaemonServiceServer::new(self)
    }

    /// Send a `DaemonMessage` to the connected Agent, if any.
    /// Returns `false` when no Agent is connected.
    pub async fn send_to_agent(&self, msg: DaemonMessage) -> bool {
        let guard = self.agent_sink.lock().await;
        if let Some(tx) = guard.as_ref() {
            tx.send(Ok(msg)).await.is_ok()
        } else {
            false
        }
    }

    /// Send a `DaemonMessage` to the connected Agent and return a oneshot
    /// receiver that fires when the Agent sends `Done` or `Error` for this
    /// `message_id`.  Returns `None` when no Agent is connected.
    pub async fn send_to_agent_and_await(
        &self,
        msg: DaemonMessage,
        message_id: &str,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.completions
            .lock()
            .await
            .insert(message_id.to_string(), tx);
        if self.send_to_agent(msg).await {
            Some(rx)
        } else {
            self.completions.lock().await.remove(message_id);
            None
        }
    }

    /// Return `true` when an Agent is currently connected.
    pub async fn is_agent_connected(&self) -> bool {
        self.agent_sink.lock().await.is_some()
    }

    /// Reply to a Feishu message indicating the Agent is not connected.
    pub async fn reply_agent_unavailable(&self, message_id: &str) {
        let _ = self
            .gateway
            .reply_text(message_id, "⚠️ Agent 未连接，请稍后再试。")
            .await;
    }
}

// ── DaemonService impl ────────────────────────────────────────────────────────

#[tonic::async_trait]
impl DaemonService for RpcServer {
    type AgentConnectStream = ReceiverStream<Result<DaemonMessage, Status>>;

    async fn agent_connect(
        &self,
        request: Request<Streaming<remi_proto::AgentMessage>>,
    ) -> Result<Response<Self::AgentConnectStream>, Status> {
        let peer = request
            .remote_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".into());
        info!(peer = %peer, "Agent connected");

        // ── Outbound channel (Daemon → Agent) ─────────────────────────────
        let (tx, rx) = mpsc::channel::<Result<DaemonMessage, Status>>(256);
        {
            let mut guard = self.agent_sink.lock().await;
            if let Some(old_tx) = guard.take() {
                // Tell the existing agent to shut down so it stops reconnecting.
                // This handles the case where a stale agent process from a
                // previous daemon run is still alive and competing with the
                // freshly-spawned one.
                warn!(peer = %peer, "replacing existing Agent connection — sending Shutdown");
                let shutdown = DaemonMessage {
                    payload: Some(remi_proto::daemon_message::Payload::Shutdown(
                        ShutdownSignal {},
                    )),
                };
                // Best-effort: ignore send errors (old agent may already be gone).
                let _ = old_tx.send(Ok(shutdown)).await;
                // Dropping old_tx here closes the old agent's stream.
            }
            *guard = Some(tx);
        }
        // Push current secrets immediately so the Agent starts with them.
        self.sync_secrets_to_agent().await;

        // ── Inbound task (Agent → Daemon) ─────────────────────────────────
        let gateway = self.gateway.clone();
        let sink = self.agent_sink.clone();
        let reactions = self.reactions.clone();
        let completions = self.completions.clone();

        tokio::spawn(async move {
            let mut inbound = request.into_inner();
            // Map: Feishu message_id → StreamingCard for that reply thread.
            let mut cards: HashMap<String, StreamingCard> = HashMap::new();

            while let Some(result) = inbound.next().await {
                let agent_msg = match result {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("gRPC stream error from Agent: {e}");
                        break;
                    }
                };

                let reply_to = agent_msg.reply_to_message_id.clone();
                debug!(reply_to = %reply_to, "received AgentMessage");

                match agent_msg.payload {
                    Some(AgentPayload::TextDelta(d)) => {
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| gateway.begin_streaming_reply(&reply_to));
                        if let Err(e) = card.push(&d.text).await {
                            warn!("StreamingCard push error: {e:#}");
                        }
                    }

                    Some(AgentPayload::Thinking(t)) => {
                        let block = format!(
                            "\n\n> 💭 *思考中…*\n> {}\n\n",
                            t.content.replace('\n', "\n> ")
                        );
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| gateway.begin_streaming_reply(&reply_to));
                        card.push(&block).await.ok();
                    }

                    Some(AgentPayload::ToolCall(tc)) => {
                        let args_preview = tc.args_json;
                        let block = format!("\n\n🔧 **{}**\n```\n{}\n```\n", tc.name, args_preview);
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| gateway.begin_streaming_reply(&reply_to));
                        card.push(&block).await.ok();
                    }

                    Some(AgentPayload::ToolResult(tr)) => {
                        let preview = truncate_chars(&tr.result, 300);
                        let block =
                            format!("✅ **{}** → `{}`\n", tr.name, preview.replace('`', "'"));
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| gateway.begin_streaming_reply(&reply_to));
                        card.push(&block).await.ok();
                    }

                    Some(AgentPayload::Stats(s)) => {
                        let secs = s.elapsed_ms / 1000;
                        let ms = s.elapsed_ms % 1000;
                        let stats = format!(
                            "\n\n---\n📊 *tokens: {}↑ {}↓ | 耗时: {}.{:03}s*",
                            s.prompt_tokens, s.completion_tokens, secs, ms
                        );
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| gateway.begin_streaming_reply(&reply_to));
                        card.push(&stats).await.ok();
                    }

                    Some(AgentPayload::Done(_)) => {
                        if let Some(mut card) = cards.remove(&reply_to) {
                            card.finish().await.ok();
                        }
                        // Remove "thinking" reaction.
                        if let Some(rid) = reactions.lock().await.remove(&reply_to) {
                            if let Err(e) = gateway.delete_reaction(&reply_to, &rid).await {
                                warn!("delete_reaction failed: {e:#}");
                            }
                        }
                        // Signal completion to queue worker.
                        if let Some(tx) = completions.lock().await.remove(&reply_to) {
                            let _ = tx.send(());
                        }
                    }

                    Some(AgentPayload::Error(e)) => {
                        let msg = format!("\n\n❌ *[错误: {}]*", e.message);
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| gateway.begin_streaming_reply(&reply_to));
                        card.push(&msg).await.ok();
                        if let Some(mut card) = cards.remove(&reply_to) {
                            card.finish().await.ok();
                        }
                        if let Some(rid) = reactions.lock().await.remove(&reply_to) {
                            if let Err(e) = gateway.delete_reaction(&reply_to, &rid).await {
                                warn!("delete_reaction failed: {e:#}");
                            }
                        }
                        // Signal completion to queue worker.
                        if let Some(tx) = completions.lock().await.remove(&reply_to) {
                            let _ = tx.send(());
                        }
                    }

                    None => {}
                }
            }

            // ── Agent disconnected ─────────────────────────────────────────
            info!(peer = %peer, "Agent disconnected");
            *sink.lock().await = None;

            // Finish any cards that were mid-stream.
            for (_, mut card) in cards {
                card.push("\n\n⚠️ *[Agent 连接断开]*").await.ok();
                card.finish().await.ok();
            }
            // Fire all pending completion signals so queue workers don't block.
            for (_, tx) in completions.lock().await.drain() {
                let _ = tx.send(());
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ── Helper: build DaemonMessage wrappers ─────────────────────────────────────

pub fn daemon_msg_im_message(ev: &im_feishu::FeishuMessage, user_uuid: &str) -> DaemonMessage {
    DaemonMessage {
        payload: Some(DaemonPayload::ImMessage(ImMessageEvent {
            message_id: ev.message_id.clone(),
            sender_open_id: user_uuid.to_string(),
            chat_id: ev.chat_id.clone(),
            chat_type: ev.chat_type.clone(),
            text: ev.text.clone(),
            images: ev.images.clone(),
            parent_id: ev.parent_id.clone().unwrap_or_default(),
        })),
    }
}

pub fn daemon_msg_im_reaction(ev: &im_feishu::FeishuReaction, user_uuid: &str) -> DaemonMessage {
    DaemonMessage {
        payload: Some(DaemonPayload::ImReaction(ImReactionEvent {
            message_id: ev.message_id.clone(),
            chat_id: ev.chat_id.clone(),
            sender_open_id: user_uuid.to_string(),
            emoji_type: ev.emoji_type.clone(),
        })),
    }
}

pub fn daemon_msg_shutdown() -> DaemonMessage {
    DaemonMessage {
        payload: Some(DaemonPayload::Shutdown(remi_proto::ShutdownSignal {})),
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Truncate a string to at most `max_chars` Unicode scalar values, appending
/// `…` if it was truncated.  Safe on any UTF-8 input.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let mut out = String::with_capacity(max_chars + 3);
    let mut count = 0;
    loop {
        match chars.next() {
            Some(c) if count < max_chars => {
                out.push(c);
                count += 1;
            }
            Some(_) => {
                out.push('…');
                break;
            }
            None => break,
        }
    }
    out
}
