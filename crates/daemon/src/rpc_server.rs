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

use im_feishu::client::{build_tool_approval_card, build_tool_approval_resolved_card};
use remi_proto::{
    AgentPayload, AgentStats, DaemonMessage, DaemonPayload, DaemonService, DaemonServiceServer,
    ImAttachmentRef, ImDocumentRef, ImMessageEvent, ImReactionEvent, SecretsSync, ShutdownSignal,
    TriggerRunEvent,
};

use crate::acp_bindings::{AcpBindingStore, AcpChannelBinding};
use crate::reply_transport::{
    encode_attachment_key, handle_im_bridge_request, ReplyTransport, StreamingReply,
};
use crate::secret_store::SecretStore;

/// Shared map: Feishu message_id → reaction_id, for cleaning up "thinking" emoji.
pub type ReactionMap = Arc<Mutex<HashMap<String, String>>>;

/// Shared map: Feishu message_id → oneshot sender, for signalling queue workers
/// when the Agent finishes processing a message.
pub type CompletionMap = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;

/// Shared map: execution_id → actual Feishu message_id used for reply routing.
pub type ReplyTargetMap = Arc<Mutex<HashMap<String, String>>>;

// ── AgentSink ─────────────────────────────────────────────────────────────────

/// Shared handle to the currently connected Agent's outbound message channel.
pub type AgentSink = Arc<Mutex<Option<mpsc::Sender<Result<DaemonMessage, Status>>>>>;

// ── RpcServer ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct RpcServer {
    transport: ReplyTransport,
    /// Shared sender — `Some` when Agent is connected.
    agent_sink: AgentSink,
    /// reaction_id keyed by message_id, for bookend cleanup on Done/Error.
    reactions: ReactionMap,
    /// oneshot completion senders keyed by message_id, for queue backpressure.
    completions: CompletionMap,
    /// Maps internal execution ids to the actual Feishu message ids they reply under.
    reply_targets: ReplyTargetMap,
    /// Shared secret store — read on connect and on manual sync.
    secret_store: Arc<RwLock<SecretStore>>,
    /// ACP channel binding registry used for IM routing.
    acp_bindings: Arc<Mutex<AcpBindingStore>>,
}

impl RpcServer {
    pub fn new(
        transport: ReplyTransport,
        secret_store: Arc<RwLock<SecretStore>>,
        acp_bindings: Arc<Mutex<AcpBindingStore>>,
    ) -> (Self, AgentSink) {
        let sink: AgentSink = Arc::new(Mutex::new(None));
        (
            Self {
                transport,
                agent_sink: sink.clone(),
                reactions: Arc::new(Mutex::new(HashMap::new())),
                completions: Arc::new(Mutex::new(HashMap::new())),
                reply_targets: Arc::new(Mutex::new(HashMap::new())),
                secret_store,
                acp_bindings,
            },
            sink,
        )
    }

    pub async fn get_acp_binding(
        &self,
        platform: &str,
        channel_id: &str,
    ) -> Option<AcpChannelBinding> {
        self.acp_bindings.lock().await.get(platform, channel_id)
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
            .max_decoding_message_size(remi_proto::GRPC_MESSAGE_LIMIT_BYTES)
            .max_encoding_message_size(remi_proto::GRPC_MESSAGE_LIMIT_BYTES)
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
        self.send_to_agent_and_await_with_reply_target(msg, message_id, message_id)
            .await
    }

    pub async fn send_to_agent_and_await_with_reply_target(
        &self,
        msg: DaemonMessage,
        execution_id: &str,
        reply_to_message_id: &str,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.completions
            .lock()
            .await
            .insert(execution_id.to_string(), tx);
        if execution_id != reply_to_message_id {
            self.reply_targets
                .lock()
                .await
                .insert(execution_id.to_string(), reply_to_message_id.to_string());
        }
        if self.send_to_agent(msg).await {
            Some(rx)
        } else {
            self.completions.lock().await.remove(execution_id);
            self.reply_targets.lock().await.remove(execution_id);
            None
        }
    }

    /// Return `true` when an Agent is currently connected.
    pub async fn is_agent_connected(&self) -> bool {
        self.agent_sink.lock().await.is_some()
    }

    /// Ask the Agent to cooperatively cancel the in-flight task for `chat_id`.
    ///
    /// The Daemon must separately ensure the corresponding completion channel
    /// is unblocked (e.g. by dropping the `oneshot::Receiver`).
    pub async fn cancel_chat(&self, chat_id: &str) {
        self.send_to_agent(DaemonMessage {
            payload: Some(DaemonPayload::ChatCancel(remi_proto::ChatCancelSignal {
                chat_id: chat_id.to_string(),
            })),
        })
        .await;
    }

    /// Reply to a Feishu message indicating the Agent is not connected.
    pub async fn reply_agent_unavailable(&self, message_id: &str) {
        let _ = self
            .transport
            .reply_text(message_id, "", "⚠️ Agent 未连接，请稍后再试。")
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
        let transport = self.transport.clone();
        let server = self.clone();
        let sink = self.agent_sink.clone();
        let reactions = self.reactions.clone();
        let completions = self.completions.clone();
        let reply_targets = self.reply_targets.clone();

        tokio::spawn(async move {
            let mut inbound = request.into_inner();
            let mut cards: HashMap<String, StreamingReply> = HashMap::new();
            let mut todo_cards: HashMap<String, String> = HashMap::new();
            let mut approval_cards: HashMap<String, String> = HashMap::new();

            while let Some(result) = inbound.next().await {
                let agent_msg = match result {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("gRPC stream error from Agent: {e}");
                        break;
                    }
                };

                let execution_id = agent_msg.reply_to_message_id.clone();
                let reply_to = reply_targets
                    .lock()
                    .await
                    .get(&execution_id)
                    .cloned()
                    .unwrap_or_else(|| execution_id.clone());
                debug!(execution_id = %execution_id, reply_to = %reply_to, "received AgentMessage");

                match agent_msg.payload {
                    Some(AgentPayload::ImBridgeRequest(req)) => {
                        let response =
                            handle_im_bridge_request(&transport, &server.acp_bindings, req).await;
                        server
                            .send_to_agent(DaemonMessage {
                                payload: Some(DaemonPayload::ImBridgeResponse(response)),
                            })
                            .await;
                    }

                    Some(AgentPayload::TextDelta(d)) => {
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| transport.begin_streaming_reply(&reply_to));
                        if let Err(e) = card.push(&d.text).await {
                            warn!("stream reply push error: {e:#}");
                        }
                    }

                    Some(AgentPayload::Thinking(t)) => {
                        let block = format!(
                            "\n\n> 💭 *思考中…*\n> {}\n\n",
                            t.content.replace('\n', "\n> ")
                        );
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| transport.begin_streaming_reply(&reply_to));
                        card.push(&block).await.ok();
                    }

                    Some(AgentPayload::ToolCall(tc)) => {
                        let args_preview = tc.args_json;
                        let block = format!("\n\n🔧 **{}**\n```\n{}\n```\n", tc.name, args_preview);
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| transport.begin_streaming_reply(&reply_to));
                        card.push(&block).await.ok();
                    }

                    Some(AgentPayload::ToolResult(tr)) => {
                        let preview = truncate_chars(&tr.result, 300);
                        let block =
                            format!("✅ **{}** → `{}`\n", tr.name, preview.replace('`', "'"));
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| transport.begin_streaming_reply(&reply_to));
                        card.push(&block).await.ok();
                    }

                    Some(AgentPayload::Stats(s)) => {
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| transport.begin_streaming_reply(&reply_to));
                        card.push(&stats_block(&s)).await.ok();
                    }

                    Some(AgentPayload::TodoState(todo_state)) => {
                        if let Some(markdown) = normalize_todo_card_markdown(&todo_state.markdown) {
                            todo_cards.insert(reply_to.clone(), markdown);
                        } else {
                            todo_cards.remove(&reply_to);
                        }
                    }

                    Some(AgentPayload::ToolApprovalRequested(request)) => {
                        let card = build_tool_approval_card(
                            &request.approval_id,
                            &request.tool_name,
                            &request.risk,
                            &request.args_summary,
                            approval_review_text(&request.review_json).as_deref(),
                        );
                        match transport.reply_card_raw_with_id(&reply_to, card).await {
                            Ok(Some(card_message_id)) => {
                                approval_cards.insert(request.approval_id, card_message_id);
                            }
                            Ok(None) => {
                                debug!("approval card sent on transport without update id");
                            }
                            Err(err) => {
                                warn!("send approval card failed: {err:#}");
                            }
                        }
                    }

                    Some(AgentPayload::ToolApprovalUpdated(request)) => {
                        let Some(card_message_id) = approval_cards.get(&request.approval_id) else {
                            continue;
                        };
                        let card = build_tool_approval_card(
                            &request.approval_id,
                            &request.tool_name,
                            &request.risk,
                            &request.args_summary,
                            approval_review_text(&request.review_json).as_deref(),
                        );
                        if let Err(err) = transport.update_card_raw(card_message_id, card).await {
                            warn!("update approval card failed: {err:#}");
                        }
                    }

                    Some(AgentPayload::ToolApprovalResolved(resolved)) => {
                        let Some(card_message_id) = approval_cards.remove(&resolved.approval_id)
                        else {
                            continue;
                        };
                        let card = build_tool_approval_resolved_card(
                            &resolved.tool_name,
                            &resolved.risk,
                            &resolved.args_summary,
                            &resolved.decision,
                        );
                        if let Err(err) = transport.update_card_raw(&card_message_id, card).await {
                            warn!("resolve approval card failed: {err:#}");
                        }
                    }

                    Some(AgentPayload::Done(_)) => {
                        finalize_reply(
                            &transport,
                            &execution_id,
                            &reply_to,
                            &mut cards,
                            &mut todo_cards,
                            &reactions,
                            &completions,
                            &reply_targets,
                        )
                        .await;
                    }

                    Some(AgentPayload::Error(e)) => {
                        let msg = format!("\n\n❌ *[错误: {}]*", e.message);
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| transport.begin_streaming_reply(&reply_to));
                        card.push(&msg).await.ok();
                        finalize_reply(
                            &transport,
                            &execution_id,
                            &reply_to,
                            &mut cards,
                            &mut todo_cards,
                            &reactions,
                            &completions,
                            &reply_targets,
                        )
                        .await;
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
            todo_cards.clear();
            // Fire all pending completion signals so queue workers don't block.
            for (_, tx) in completions.lock().await.drain() {
                let _ = tx.send(());
            }
            reply_targets.lock().await.clear();
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ── Helper: build DaemonMessage wrappers ─────────────────────────────────────

pub fn daemon_msg_im_message(
    ev: &im_feishu::FeishuMessage,
    platform: &str,
    user_uuid: &str,
    sender_username: Option<&str>,
    todo_create_via_sdk: bool,
    trigger_tools_enabled: bool,
    acp_session_id: Option<&str>,
) -> DaemonMessage {
    info!(
        message_id = %ev.message_id,
        sender_user_id = %user_uuid,
        sender_username = sender_username.unwrap_or(""),
        has_sender_username = sender_username.map(str::trim).map(|value| !value.is_empty()).unwrap_or(false),
        todo_create_via_sdk,
        trigger_tools_enabled,
        "daemon_msg_im_message: building ImMessageEvent"
    );
    DaemonMessage {
        payload: Some(DaemonPayload::ImMessage(ImMessageEvent {
            message_id: ev.message_id.clone(),
            sender_user_id: user_uuid.to_string(),
            chat_id: ev.chat_id.clone(),
            chat_type: ev.chat_type.clone(),
            text: ev.text.clone(),
            images: ev.images.clone(),
            parent_id: ev.parent_id.clone().unwrap_or_default(),
            platform: platform.to_string(),
            attachments: ev
                .files
                .iter()
                .map(|file| {
                    debug!(
                        file_key  = %file.file_key,
                        file_name = %file.file_name,
                        file_type = %file.file_type,
                        mime_type = %file.mime_type,
                        "daemon_msg_im_message: attachment"
                    );
                    ImAttachmentRef {
                        key: encode_attachment_key(&ev.message_id, &file.file_key),
                        name: file.file_name.clone(),
                        mime_type: file.mime_type.clone(),
                        size_bytes: file.size_bytes,
                        file_type: file.file_type.clone(),
                    }
                })
                .collect(),
            documents: ev
                .documents
                .iter()
                .map(|doc| ImDocumentRef {
                    url: doc.url.clone(),
                    title: doc.title.clone(),
                    doc_type: doc.doc_type.clone(),
                    token: doc.token.clone(),
                })
                .collect(),
            sender_username: sender_username.unwrap_or_default().to_string(),
            todo_create_via_sdk,
            trigger_tools_enabled,
            acp_session_id: acp_session_id.unwrap_or_default().to_string(),
        })),
    }
}

pub fn daemon_msg_im_reaction(ev: &im_feishu::FeishuReaction, user_uuid: &str) -> DaemonMessage {
    DaemonMessage {
        payload: Some(DaemonPayload::ImReaction(ImReactionEvent {
            message_id: ev.message_id.clone(),
            chat_id: ev.chat_id.clone(),
            sender_user_id: user_uuid.to_string(),
            emoji_type: ev.emoji_type.clone(),
        })),
    }
}

pub fn daemon_msg_trigger_run(
    execution_id: &str,
    trigger_uuid: &str,
    trigger_name: &str,
    reply_to_message_id: &str,
    sender_user_id: &str,
    sender_username: Option<&str>,
    chat_id: &str,
    chat_type: Option<&str>,
    request: &str,
    platform: Option<&str>,
    todo_create_via_sdk: bool,
    trigger_tools_enabled: bool,
) -> DaemonMessage {
    DaemonMessage {
        payload: Some(DaemonPayload::TriggerRun(TriggerRunEvent {
            execution_id: execution_id.to_string(),
            trigger_uuid: trigger_uuid.to_string(),
            trigger_name: trigger_name.to_string(),
            reply_to_message_id: reply_to_message_id.to_string(),
            sender_user_id: sender_user_id.to_string(),
            sender_username: sender_username.unwrap_or_default().to_string(),
            chat_id: chat_id.to_string(),
            chat_type: chat_type.unwrap_or_default().to_string(),
            request: request.to_string(),
            platform: platform.unwrap_or_default().to_string(),
            todo_create_via_sdk,
            trigger_tools_enabled,
        })),
    }
}

#[allow(dead_code)]
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

fn normalize_todo_card_markdown(markdown: &str) -> Option<String> {
    let trimmed = markdown.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

async fn finalize_reply(
    transport: &ReplyTransport,
    execution_id: &str,
    reply_to: &str,
    cards: &mut HashMap<String, StreamingReply>,
    todo_cards: &mut HashMap<String, String>,
    reactions: &ReactionMap,
    completions: &CompletionMap,
    reply_targets: &ReplyTargetMap,
) {
    let todo_card = todo_cards.remove(reply_to);

    if let Some(mut card) = cards.remove(reply_to) {
        card.finish().await.ok();
    }

    if let Some(markdown) = todo_card {
        if let Err(err) = transport.reply_card(reply_to, &markdown).await {
            warn!(reply_to, "reply_card for todo state failed: {err:#}");
        }
    }

    if let Some(rid) = reactions.lock().await.remove(reply_to) {
        if let Err(err) = transport.delete_reaction(reply_to, &rid).await {
            warn!("delete_reaction failed: {err:#}");
        }
    }

    if let Some(tx) = completions.lock().await.remove(execution_id) {
        let _ = tx.send(());
    }
    reply_targets.lock().await.remove(execution_id);
}

fn stats_block(stats: &AgentStats) -> String {
    let secs = stats.elapsed_ms / 1000;
    let ms = stats.elapsed_ms % 1000;
    format!(
        "\n\n---\n📊 *tokens: {}↑ {}↓ | 耗时: {}.{:03}s*",
        stats.prompt_tokens, stats.completion_tokens, secs, ms
    )
}

fn approval_review_text(review_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(review_json).ok()?;
    let reason = value
        .get("reason")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let concerns = value
        .get("concerns")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut text = String::new();
    if !reason.trim().is_empty() {
        text.push_str(reason.trim());
    }
    if !concerns.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str("Concerns:");
        for concern in concerns {
            text.push_str("\n- ");
            text.push_str(concern);
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::{normalize_todo_card_markdown, stats_block};
    use crate::reply_transport::encode_attachment_key;
    use remi_proto::AgentStats;

    #[test]
    fn encode_agent_file_key_tolerates_double_backslashes() {
        assert_eq!(
            encode_attachment_key("msg_123", "msg_123\\\\file_abc"),
            "msg_123\\file_abc"
        );
    }

    #[test]
    fn todo_card_markdown_is_kept_as_a_separate_reply_body() {
        let todo =
            normalize_todo_card_markdown("\n📝 **当前 Todo**\n```\n[○] 1 Draft changelog\n```\n")
                .expect("todo markdown should be kept");

        assert_eq!(todo, "📝 **当前 Todo**\n```\n[○] 1 Draft changelog\n```");
    }

    #[test]
    fn stats_block_does_not_include_todo_content() {
        let stats = stats_block(&AgentStats {
            prompt_tokens: 120,
            completion_tokens: 45,
            elapsed_ms: 3456,
        });

        assert_eq!(stats, "\n\n---\n📊 *tokens: 120↑ 45↓ | 耗时: 3.456s*");
    }
}
