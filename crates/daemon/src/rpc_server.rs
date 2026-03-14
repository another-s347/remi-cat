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
    AgentPayload, AgentStats, DaemonMessage, DaemonPayload, DaemonService, DaemonServiceServer,
    ImAttachmentRef, ImBridgeRequest, ImBridgeResponse, ImDocumentRef, ImDownloadedFile,
    ImMessageEvent, ImReactionEvent, ImUploadedFile, SecretsSync, ShutdownSignal,
};

use crate::secret_store::SecretStore;

const AGENT_FILE_KEY_SEPARATOR: char = '\\';
const LEGACY_AGENT_FILE_KEY_SEPARATOR: char = '\t';

/// Shared map: Feishu message_id → reaction_id, for cleaning up "thinking" emoji.
pub type ReactionMap = Arc<Mutex<HashMap<String, String>>>;

/// Shared map: Feishu message_id → oneshot sender, for signalling queue workers
/// when the Agent finishes processing a message.
pub type CompletionMap = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;

fn decode_agent_file_key(value: &str) -> Option<(&str, &str)> {
    let value = value.trim();
    for separator in [AGENT_FILE_KEY_SEPARATOR, LEGACY_AGENT_FILE_KEY_SEPARATOR] {
        if let Some((message_id, file_key)) = value.split_once(separator) {
            let message_id = message_id.trim();
            let file_key = file_key.trim().trim_start_matches(|c| {
                c == AGENT_FILE_KEY_SEPARATOR || c == LEGACY_AGENT_FILE_KEY_SEPARATOR
            });
            if !message_id.is_empty() && !file_key.is_empty() {
                return Some((message_id, file_key));
            }
        }
    }
    None
}

fn encode_agent_file_key(message_id: &str, file_key: &str) -> String {
    let message_id = message_id.trim();
    let file_key = file_key.trim();
    if let Some((owner_message_id, real_file_key)) = decode_agent_file_key(file_key) {
        return format!(
            "{}{}{}",
            owner_message_id, AGENT_FILE_KEY_SEPARATOR, real_file_key
        );
    }
    if message_id.is_empty() || file_key.is_empty() {
        return file_key.to_string();
    }
    format!("{message_id}{AGENT_FILE_KEY_SEPARATOR}{file_key}")
}

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
        let server = self.clone();
        let sink = self.agent_sink.clone();
        let reactions = self.reactions.clone();
        let completions = self.completions.clone();

        tokio::spawn(async move {
            let mut inbound = request.into_inner();
            // Map: Feishu message_id → StreamingCard for that reply thread.
            let mut cards: HashMap<String, StreamingCard> = HashMap::new();
            let mut todo_cards: HashMap<String, String> = HashMap::new();

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
                    Some(AgentPayload::ImBridgeRequest(req)) => {
                        let response = handle_im_bridge_request(&gateway, req).await;
                        server
                            .send_to_agent(DaemonMessage {
                                payload: Some(DaemonPayload::ImBridgeResponse(response)),
                            })
                            .await;
                    }

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
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| gateway.begin_streaming_reply(&reply_to));
                        card.push(&stats_block(&s)).await.ok();
                    }

                    Some(AgentPayload::TodoState(todo_state)) => {
                        if let Some(markdown) = normalize_todo_card_markdown(&todo_state.markdown) {
                            todo_cards.insert(reply_to.clone(), markdown);
                        } else {
                            todo_cards.remove(&reply_to);
                        }
                    }

                    Some(AgentPayload::Done(_)) => {
                        finalize_reply(
                            &gateway,
                            &reply_to,
                            &mut cards,
                            &mut todo_cards,
                            &reactions,
                            &completions,
                        )
                        .await;
                    }

                    Some(AgentPayload::Error(e)) => {
                        let msg = format!("\n\n❌ *[错误: {}]*", e.message);
                        let card = cards
                            .entry(reply_to.clone())
                            .or_insert_with(|| gateway.begin_streaming_reply(&reply_to));
                        card.push(&msg).await.ok();
                        finalize_reply(
                            &gateway,
                            &reply_to,
                            &mut cards,
                            &mut todo_cards,
                            &reactions,
                            &completions,
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
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ── Helper: build DaemonMessage wrappers ─────────────────────────────────────

async fn handle_im_bridge_request(
    gateway: &FeishuGateway,
    request: ImBridgeRequest,
) -> ImBridgeResponse {
    let request_id = request.request_id;
    let Some(payload) = request.payload else {
        return ImBridgeResponse {
            request_id,
            error: "missing bridge payload".into(),
            payload: None,
        };
    };

    match payload {
        remi_proto::im_bridge_request::Payload::Download(download) => {
            if download.platform != "feishu" {
                return ImBridgeResponse {
                    request_id,
                    error: format!("unsupported platform: {}", download.platform),
                    payload: None,
                };
            }

            let result = if !download.attachment_key.is_empty() {
                // Self-contained keys encode the owner message ID so the
                // agent never needs to pass message_id explicitly.
                let (owner_msg_id, real_key) = decode_agent_file_key(&download.attachment_key)
                    .map(|(owner, key)| (owner.to_string(), key.to_string()))
                    .unwrap_or_else(|| {
                        (download.message_id.clone(), download.attachment_key.clone())
                    });
                debug!(
                    message_id = %owner_msg_id,
                    file_key   = %real_key,
                    file_type  = %download.file_type,
                    "handle_im_bridge_request: download attachment"
                );
                gateway
                    .download_file(&owner_msg_id, &real_key, &download.file_type)
                    .await
                    .map(|(mime_type, file_name, content)| ImDownloadedFile {
                        file_name,
                        mime_type,
                        content,
                        source_label: format!("attachment:{}", real_key),
                    })
            } else if !download.document_url.is_empty() {
                gateway.download_document(&download.document_url).await.map(
                    |(mime_type, file_name, content)| ImDownloadedFile {
                        file_name,
                        mime_type,
                        content,
                        source_label: download.document_url.clone(),
                    },
                )
            } else {
                Err(anyhow::anyhow!(
                    "download request must specify attachment_key or document_url"
                ))
            };

            match result {
                Ok(downloaded) => ImBridgeResponse {
                    request_id,
                    error: String::new(),
                    payload: Some(remi_proto::im_bridge_response::Payload::Downloaded(
                        downloaded,
                    )),
                },
                Err(e) => ImBridgeResponse {
                    request_id,
                    error: e.to_string(),
                    payload: None,
                },
            }
        }
        remi_proto::im_bridge_request::Payload::Upload(upload) => {
            if upload.platform != "feishu" {
                return ImBridgeResponse {
                    request_id,
                    error: format!("unsupported platform: {}", upload.platform),
                    payload: None,
                };
            }

            let result = async {
                let file_key = gateway
                    .upload_file(
                        &upload.file_name,
                        &upload.mime_type,
                        &upload.content,
                        &upload.file_type,
                    )
                    .await?;
                let sent_message_id = if !upload.message_id.is_empty() {
                    match gateway
                        .reply_file(&upload.message_id, &file_key, &upload.file_type)
                        .await
                    {
                        Ok(message_id) => message_id,
                        Err(err) => {
                            warn!(error = %err, "reply_file failed, falling back to send_file");
                            gateway
                                .send_file(&upload.chat_id, &file_key, &upload.file_type)
                                .await?
                        }
                    }
                } else {
                    gateway
                        .send_file(&upload.chat_id, &file_key, &upload.file_type)
                        .await?
                };

                Ok::<ImUploadedFile, anyhow::Error>(ImUploadedFile {
                    file_name: upload.file_name.clone(),
                    file_key: file_key.clone(),
                    message_id: sent_message_id.clone(),
                    resource_url: gateway.file_resource_url(
                        &sent_message_id,
                        &file_key,
                        &upload.file_type,
                    ),
                })
            }
            .await;

            match result {
                Ok(uploaded) => ImBridgeResponse {
                    request_id,
                    error: String::new(),
                    payload: Some(remi_proto::im_bridge_response::Payload::Uploaded(uploaded)),
                },
                Err(e) => ImBridgeResponse {
                    request_id,
                    error: e.to_string(),
                    payload: None,
                },
            }
        }
    }
}

pub fn daemon_msg_im_message(
    ev: &im_feishu::FeishuMessage,
    user_uuid: &str,
    sender_username: Option<&str>,
) -> DaemonMessage {
    info!(
        message_id = %ev.message_id,
        sender_user_id = %user_uuid,
        sender_username = sender_username.unwrap_or(""),
        has_sender_username = sender_username.map(str::trim).map(|value| !value.is_empty()).unwrap_or(false),
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
            platform: "feishu".into(),
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
                        key: encode_agent_file_key(&ev.message_id, &file.file_key),
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
    gateway: &FeishuGateway,
    reply_to: &str,
    cards: &mut HashMap<String, StreamingCard>,
    todo_cards: &mut HashMap<String, String>,
    reactions: &ReactionMap,
    completions: &CompletionMap,
) {
    let todo_card = todo_cards.remove(reply_to);

    if let Some(mut card) = cards.remove(reply_to) {
        card.finish().await.ok();
    }

    if let Some(markdown) = todo_card {
        if let Err(err) = gateway.reply_card(reply_to, &markdown).await {
            warn!(reply_to, "reply_card for todo state failed: {err:#}");
        }
    }

    if let Some(rid) = reactions.lock().await.remove(reply_to) {
        if let Err(err) = gateway.delete_reaction(reply_to, &rid).await {
            warn!("delete_reaction failed: {err:#}");
        }
    }

    if let Some(tx) = completions.lock().await.remove(reply_to) {
        let _ = tx.send(());
    }
}

fn stats_block(stats: &AgentStats) -> String {
    let secs = stats.elapsed_ms / 1000;
    let ms = stats.elapsed_ms % 1000;
    format!(
        "\n\n---\n📊 *tokens: {}↑ {}↓ | 耗时: {}.{:03}s*",
        stats.prompt_tokens, stats.completion_tokens, secs, ms
    )
}

#[cfg(test)]
mod tests {
    use super::{decode_agent_file_key, normalize_todo_card_markdown, stats_block};
    use remi_proto::AgentStats;

    #[test]
    fn decode_agent_file_key_tolerates_double_backslashes() {
        let (message_id, file_key) =
            decode_agent_file_key("msg_123\\\\file_abc").expect("double escaped key should decode");

        assert_eq!(message_id, "msg_123");
        assert_eq!(file_key, "file_abc");
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
