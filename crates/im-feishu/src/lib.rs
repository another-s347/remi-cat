//! `im-feishu` — Feishu IM platform adapter (standalone).
//!
//! Uses **WebSocket long connection** for inbound events and Feishu REST APIs
//! for outbound messages and card updates.
//!
//! # Wire protocol
//!
//! Feishu sends **binary** WebSocket frames whose body is a protobuf-serialised
//! [`frame::PbFrame`].  The `method` field distinguishes frame classes:
//!
//! | method | class   | header `type`        | action                          |
//! |--------|---------|----------------------|---------------------------------|
//! | 0      | control | `ping`               | reply with pong                 |
//! | 0      | control | `pong`               | ignore                          |
//! | 1      | data    | `event`              | parse payload, ACK, forward     |
//! | 1      | data    | `card`               | parse card action, ACK, forward |
//!
//! The ACK for a data frame is the same `PbFrame` echoed back with
//! `payload = {"code":200}`.
//!
//! # Async ACK
//!
//! Feishu requires the ACK within **3 seconds**.  The WS reader task sends the
//! ACK *before* forwarding the event to the channel so the bot's processing
//! time is never bounded by the deadline.

pub mod client;
pub mod frame;
pub mod streaming;

use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

pub use crate::client::FeishuClient;
use crate::frame::PbFrame;
pub use crate::streaming::StreamingCard;

// ── Frame method constants ────────────────────────────────────────────────────

const METHOD_CONTROL: i32 = 0;
const METHOD_DATA: i32 = 1;

const TYPE_PING: &str = "ping";
const TYPE_PONG: &str = "pong";
const TYPE_EVENT: &str = "event";
const TYPE_CARD: &str = "card";

// ── Public event types ────────────────────────────────────────────────────────

/// An event received from the Feishu platform.
#[derive(Debug, Clone)]
pub enum FeishuEvent {
    MessageReceived(FeishuMessage),
    ReactionReceived(FeishuReaction),
    /// An interactive card button/form submit action triggered by a user.
    CardAction {
        /// The `message_id` of the card that was interacted with.
        card_message_id: String,
        /// The `value` object attached to the button/form.
        action_value: serde_json::Value,
        /// The `open_id` of the user who triggered the action.
        user_open_id: String,
    },
    Unknown {
        event_type: String,
        raw: serde_json::Value,
    },
}

/// A message received from a Feishu user (text, image, or post/rich-text).
#[derive(Debug, Clone)]
pub struct FeishuMessage {
    pub message_id: String,
    pub sender_user_id: String,
    pub chat_id: String,
    /// `"p2p"` for direct messages, `"group"` for group chats.
    pub chat_type: String,
    /// Plain-text body (empty for image-only messages).
    /// In group chats, the bot's @-mention placeholder has already been stripped.
    pub text: String,
    /// Image keys for any images attached to the message.
    pub images: Vec<String>,
    /// File attachments carried by this message.
    pub files: Vec<FeishuFile>,
    /// Feishu document links discovered in this message.
    pub documents: Vec<FeishuDocument>,
    /// Parent message ID when this message quotes/replies to another.
    pub parent_id: Option<String>,
    /// `true` when this message @-mentions the bot (always `true` for p2p).
    pub at_bot: bool,
    /// Non-bot @mentions preserved from the original event payload.
    pub mentions: Vec<FeishuMention>,
}

#[derive(Debug, Clone, Default)]
pub struct FeishuFile {
    pub file_key: String,
    pub file_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    /// Feishu IM message type: `"file"`, `"folder"`, etc.
    pub file_type: String,
}

#[derive(Debug, Clone, Default)]
pub struct FeishuDocument {
    pub url: String,
    pub title: String,
    pub doc_type: String,
    pub token: String,
}

#[derive(Debug, Clone, Default)]
pub struct FeishuMention {
    pub key: String,
    pub open_id: Option<String>,
    pub user_id: Option<String>,
}

/// An emoji reaction added to a message by a user.
#[derive(Debug, Clone)]
pub struct FeishuReaction {
    pub message_id: String,
    pub chat_id: String,
    pub sender_user_id: String,
    pub emoji_type: String,
}

// ── Event payload JSON types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct EventEnvelope {
    #[allow(dead_code)]
    schema: Option<String>,
    header: Option<EventHeader>,
    event: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct EventHeader {
    event_type: String,
    #[allow(dead_code)]
    event_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageReceiveEvent {
    message: RawMessage,
    sender: RawSender,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    message_id: String,
    chat_id: String,
    chat_type: String,
    message_type: String,
    content: Option<String>,
    parent_id: Option<String>,
    #[serde(default)]
    mentions: Vec<RawMention>,
}

#[derive(Debug, Deserialize)]
struct RawMention {
    /// Placeholder key in the text, e.g. `"@_user_1"`.
    key: String,
    id: RawSenderId,
}

#[derive(Debug, Deserialize)]
struct ImageContent {
    image_key: String,
}

#[derive(Debug, Deserialize)]
struct FileContent {
    file_key: String,
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    mime_type: String,
    #[serde(default)]
    file_size: u64,
}

#[derive(Debug, Deserialize)]
struct RawSender {
    sender_id: RawSenderId,
}

#[derive(Debug, Deserialize)]
struct RawSenderId {
    open_id: Option<String>,
    user_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TextContent {
    text: String,
}

// ── FeishuGateway ─────────────────────────────────────────────────────────────

/// Feishu IM gateway using WebSocket long connection + REST APIs.
///
/// `FeishuGateway` is cheap to clone — the inner [`FeishuClient`] shares its
/// token state via `Arc`.
#[derive(Clone)]
pub struct FeishuGateway {
    client: FeishuClient,
}

impl FeishuGateway {
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            client: FeishuClient::new(app_id, app_secret),
        }
    }

    /// Acquire token, fetch bot info, spawn the WS reader task, return the event channel.
    pub async fn start(&self) -> Result<mpsc::Receiver<FeishuEvent>> {
        self.client.refresh_token().await?;

        if let Err(e) = self.client.init_bot_info().await {
            warn!("failed to fetch bot info (group @mention filter disabled): {e:#}");
        };

        let (tx, rx) = mpsc::channel::<FeishuEvent>(256);
        let this = self.clone();

        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            const MAX_BACKOFF: Duration = Duration::from_secs(60);

            loop {
                match this.run_session(&tx).await {
                    Ok(()) => {
                        info!("WS session ended — reconnecting");
                        backoff = Duration::from_secs(1);
                    }
                    Err(e) => {
                        error!("WS error: {e:#} — reconnecting in {backoff:?}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        if let Err(te) = this.client.refresh_token().await {
                            error!("token refresh failed: {te:#}");
                        }
                    }
                }

                if tx.is_closed() {
                    info!("event receiver dropped — stopping WS task");
                    break;
                }
            }
        });

        Ok(rx)
    }

    /// Run one WebSocket session until it closes or an error occurs.
    async fn run_session(&self, tx: &mpsc::Sender<FeishuEvent>) -> Result<()> {
        let endpoint = self.client.ws_endpoint().await?;
        info!("connecting Feishu WS: {}", endpoint.url);

        let (ws, _) = connect_async(&endpoint.url).await?;
        info!("WS connected");

        let (mut sink, mut stream) = ws.split();

        // Fetch bot_open_id once per session for @mention filtering.
        let bot_open_id = self.client.get_bot_open_id().await;

        while let Some(msg) = stream.next().await {
            let bytes = match msg? {
                Message::Binary(b) => b,
                Message::Ping(d) => {
                    let _ = sink.send(Message::Pong(d)).await;
                    continue;
                }
                Message::Close(_) => {
                    info!("WS closed by server");
                    break;
                }
                other => {
                    debug!("ignoring WS message type: {:?}", other.is_text());
                    continue;
                }
            };

            let frame = match PbFrame::decode(bytes.as_ref()) {
                Ok(f) => f,
                Err(e) => {
                    warn!("protobuf decode error: {e}");
                    continue;
                }
            };

            match frame.method {
                METHOD_CONTROL => {
                    let t = frame.get_header("type").unwrap_or("");
                    match t {
                        TYPE_PING => {
                            // Reply with a pong (same frame, headers preserved).
                            let pong = PbFrame {
                                seq_id: frame.seq_id,
                                log_id: frame.log_id,
                                service: frame.service,
                                method: METHOD_CONTROL,
                                headers: {
                                    let mut h = frame.headers.clone();
                                    // Update type header to "pong".
                                    for hdr in &mut h {
                                        if hdr.key == "type" {
                                            hdr.value = TYPE_PONG.into();
                                        }
                                    }
                                    h
                                },
                                ..Default::default()
                            };
                            let mut buf = Vec::new();
                            if prost::Message::encode(&pong, &mut buf).is_ok() {
                                let _ = sink.send(Message::Binary(buf.into())).await;
                            }
                        }
                        TYPE_PONG => {
                            debug!("received pong");
                        }
                        other => {
                            debug!("unknown control frame type: {other}");
                        }
                    }
                }

                METHOD_DATA => {
                    // ACK immediately — must happen within 3 s.
                    let ack_bytes = frame.encode_ack();
                    let _ = sink.send(Message::Binary(ack_bytes.into())).await;

                    let msg_type = frame.get_header("type").unwrap_or("");
                    debug!(msg_type, seq = frame.seq_id, "METHOD_DATA frame received");

                    let payload = match &frame.payload {
                        Some(p) if !p.is_empty() => p,
                        _ => {
                            debug!("METHOD_DATA frame has empty payload — skipping");
                            continue;
                        }
                    };

                    let event = if msg_type == TYPE_EVENT {
                        debug!(bytes = payload.len(), "dispatching as event");
                        Self::extract_event(payload, bot_open_id.as_deref())
                    } else if msg_type == TYPE_CARD {
                        debug!(bytes = payload.len(), "dispatching as card action");
                        if tracing::enabled!(tracing::Level::TRACE) {
                            if let Ok(s) = std::str::from_utf8(payload) {
                                tracing::trace!(raw = s, "card action payload");
                            }
                        }
                        Self::extract_card_action(payload)
                    } else {
                        debug!(msg_type, "unknown METHOD_DATA type — skipping");
                        continue;
                    };

                    if let Some(event) = event {
                        if tx.send(event).await.is_err() {
                            return Ok(()); // receiver dropped
                        }
                    }
                }

                other => {
                    debug!("unknown frame method: {other}");
                }
            }
        }

        Ok(())
    }

    fn extract_event(payload: &[u8], bot_open_id: Option<&str>) -> Option<FeishuEvent> {
        let envelope: EventEnvelope = serde_json::from_slice(payload)
            .map_err(|e| warn!("event JSON parse error: {e}"))
            .ok()?;

        let header = envelope.header?;
        let event_body = envelope.event?;

        match header.event_type.as_str() {
            "im.message.receive_v1" => {
                let ev: MessageReceiveEvent = serde_json::from_value(event_body).ok()?;
                let msg = &ev.message;

                let content_str = msg.content.as_deref().unwrap_or("");
                debug!(
                    message_id = %msg.message_id,
                    chat_id = %msg.chat_id,
                    chat_type = %msg.chat_type,
                    message_type = %msg.message_type,
                    content_len = content_str.len(),
                    content_preview = %preview_message_content(content_str),
                    mention_count = msg.mentions.len(),
                    "extract_event: raw Feishu message"
                );
                let (raw_text, images, files, content_documents) = match msg.message_type.as_str() {
                    "text" => {
                        let tc: TextContent = match serde_json::from_str(content_str) {
                            Ok(tc) => tc,
                            Err(e) => {
                                warn!(
                                    message_id = %msg.message_id,
                                    content_preview = %preview_message_content(content_str),
                                    "extract_event: failed to parse text content: {e}"
                                );
                                return None;
                            }
                        };
                        (tc.text.trim().to_string(), vec![], vec![], vec![])
                    }
                    "image" => {
                        let ic: ImageContent = match serde_json::from_str(content_str) {
                            Ok(ic) => ic,
                            Err(e) => {
                                warn!(
                                    message_id = %msg.message_id,
                                    content_preview = %preview_message_content(content_str),
                                    "extract_event: failed to parse image content: {e}"
                                );
                                return None;
                            }
                        };
                        (String::new(), vec![ic.image_key], vec![], vec![])
                    }
                    "file" | "folder" => {
                        let fc: FileContent = match serde_json::from_str(content_str) {
                            Ok(fc) => fc,
                            Err(e) => {
                                warn!(
                                    message_id = %msg.message_id,
                                    content_preview = %preview_message_content(content_str),
                                    "extract_event: failed to parse file content: {e}"
                                );
                                return None;
                            }
                        };
                        let ftype = msg.message_type.clone();
                        (
                            String::new(),
                            vec![],
                            vec![FeishuFile {
                                file_key: fc.file_key,
                                file_name: fc.file_name,
                                mime_type: fc.mime_type,
                                size_bytes: fc.file_size,
                                file_type: ftype,
                            }],
                            vec![],
                        )
                    }
                    "post" => extract_from_post(content_str),
                    other => {
                        debug!(
                            message_id = %msg.message_id,
                            message_type = other,
                            content_preview = %preview_message_content(content_str),
                            "extract_event: unsupported Feishu message type"
                        );
                        return None;
                    }
                };

                // Determine if the bot was @mentioned, and strip the mention
                // placeholder from the text so the LLM sees clean input.
                let (text, at_bot) = strip_bot_mention(&raw_text, &msg.mentions, bot_open_id);
                let documents =
                    merge_documents(content_documents, extract_doc_links_from_text(&text));

                debug!(
                    message_id = %msg.message_id,
                    message_type = %msg.message_type,
                    raw_text_len = raw_text.chars().count(),
                    text_len = text.chars().count(),
                    image_key_count = images.len(),
                    file_count = files.len(),
                    document_count = documents.len(),
                    at_bot,
                    "extract_event: normalized Feishu message"
                );

                // P2P messages always address the bot.
                let at_bot = at_bot || msg.chat_type != "group";

                fn preview_message_content(content: &str) -> &str {
                    const MAX_PREVIEW: usize = 160;
                    if content.len() <= MAX_PREVIEW {
                        return content;
                    }

                    let mut end = MAX_PREVIEW;
                    while !content.is_char_boundary(end) {
                        end -= 1;
                    }
                    &content[..end]
                }
                let sender_id = ev
                    .sender
                    .sender_id
                    .open_id
                    .or(ev.sender.sender_id.user_id)
                    .unwrap_or_default();
                let mentions = msg
                    .mentions
                    .iter()
                    .filter(|mention| mention.id.open_id.as_deref() != bot_open_id)
                    .map(|mention| FeishuMention {
                        key: mention.key.clone(),
                        open_id: mention.id.open_id.clone(),
                        user_id: mention.id.user_id.clone(),
                    })
                    .collect();

                Some(FeishuEvent::MessageReceived(FeishuMessage {
                    message_id: msg.message_id.clone(),
                    sender_user_id: sender_id,
                    chat_id: msg.chat_id.clone(),
                    chat_type: msg.chat_type.clone(),
                    text,
                    images,
                    files,
                    documents,
                    parent_id: msg.parent_id.clone(),
                    at_bot,
                    mentions,
                }))
            }

            "im.message.reaction.created_v1" => {
                // Ignore reactions added by the bot itself.
                if event_body.get("operator_type").and_then(|v| v.as_str()) != Some("user") {
                    return None;
                }
                let message_id = event_body.get("message_id")?.as_str()?.to_string();
                let chat_id = event_body
                    .get("chat_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_default();
                let sender_user_id = event_body
                    .get("user_id")?
                    .get("open_id")?
                    .as_str()?
                    .to_string();
                let emoji_type = event_body
                    .get("reaction_type")?
                    .get("emoji_type")?
                    .as_str()?
                    .to_string();

                Some(FeishuEvent::ReactionReceived(FeishuReaction {
                    message_id,
                    chat_id,
                    sender_user_id,
                    emoji_type,
                }))
            }

            "card.action.trigger" => {
                debug!(event_body = ?event_body, "card.action.trigger received");
                // open_id is nested under event.operator.open_id.
                let open_id = event_body
                    .get("operator")
                    .and_then(|op| op.get("open_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                // open_message_id is nested under event.context.open_message_id.
                let context = match event_body.get("context") {
                    Some(c) => c,
                    None => {
                        warn!(event_body = ?event_body, "card.action.trigger missing context");
                        return None;
                    }
                };
                let open_message_id = match context.get("open_message_id").and_then(|v| v.as_str())
                {
                    Some(id) => id.to_string(),
                    None => {
                        warn!(context = ?context, "card.action.trigger missing context.open_message_id");
                        return None;
                    }
                };
                let action = match event_body.get("action") {
                    Some(a) => a,
                    None => {
                        warn!(event_body = ?event_body, "card.action.trigger missing action");
                        return None;
                    }
                };
                debug!(action = ?action, "card action object");
                // For schema 2.0 with behaviors, the value may come from
                // action.value directly or may be absent — fall back to empty object.
                let mut action_value = action
                    .get("value")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                // Merge form_value fields so callers can read input values directly.
                if let Some(form_value) = action.get("form_value") {
                    debug!(form_value = ?form_value, "merging form_value");
                    if let (Some(av), Some(fv)) =
                        (action_value.as_object_mut(), form_value.as_object())
                    {
                        for (k, val) in fv {
                            av.entry(k).or_insert_with(|| val.clone());
                        }
                    }
                }
                debug!(action_value = ?action_value, open_message_id, "card.action.trigger parsed OK");
                Some(FeishuEvent::CardAction {
                    card_message_id: open_message_id,
                    action_value,
                    user_open_id: open_id,
                })
            }

            _ => Some(FeishuEvent::Unknown {
                event_type: header.event_type,
                raw: event_body,
            }),
        }
    }

    /// Parse a Feishu card callback payload and return a [`FeishuEvent::CardAction`].
    ///
    /// Feishu sends a JSON body like:
    /// ```json
    /// {
    ///   "operator": { "open_id": "ou_..." },
    ///   "context": { "open_message_id": "om_...", "open_chat_id": "oc_..." },
    ///   "action": { "value": { ... }, "tag": "button" }
    /// }
    /// ```
    fn extract_card_action(payload: &[u8]) -> Option<FeishuEvent> {
        let v: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|e| warn!("card action JSON parse error: {e}"))
            .ok()?;
        debug!(raw = ?v, "extract_card_action: parsed JSON");
        // open_id lives under operator.open_id.
        let user_open_id = v
            .get("operator")
            .and_then(|op| op.get("open_id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        // open_message_id lives under context.open_message_id.
        let card_message_id = match v
            .get("context")
            .and_then(|c| c.get("open_message_id"))
            .and_then(|v| v.as_str())
        {
            Some(id) => id.to_string(),
            None => {
                warn!(
                    "card action missing context.open_message_id; top-level keys: {:?}",
                    v.as_object().map(|o| o.keys().collect::<Vec<_>>())
                );
                return None;
            }
        };
        let action = match v.get("action") {
            Some(a) => a,
            None => {
                warn!("card action missing 'action' field");
                return None;
            }
        };
        // Merge form_value fields into the button value so callers can read
        // input fields directly from action_value["new_key"] etc.
        let mut action_value = action
            .get("value")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(form_value) = action.get("form_value") {
            if let (Some(av), Some(fv)) = (action_value.as_object_mut(), form_value.as_object()) {
                for (k, val) in fv {
                    av.entry(k).or_insert_with(|| val.clone());
                }
            }
        }
        Some(FeishuEvent::CardAction {
            card_message_id,
            action_value,
            user_open_id,
        })
    }

    // ── Outbound helpers ──────────────────────────────────────────────────────

    pub async fn get_user_name(&self, user_id: &str) -> Result<Option<String>> {
        self.client.get_user_name(user_id).await
    }

    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String> {
        self.client.send_text(chat_id, text).await
    }

    pub async fn reply_text(&self, message_id: &str, text: &str) -> Result<String> {
        self.client.reply_text(message_id, text).await
    }

    pub async fn send_card(&self, chat_id: &str, text: &str) -> Result<String> {
        self.client.send_card(chat_id, text).await
    }

    pub async fn reply_card(&self, message_id: &str, text: &str) -> Result<String> {
        self.client.reply_card(message_id, text).await
    }

    /// Reply to a message with a fully-built card JSON value.
    /// Returns the new card `message_id`.
    pub async fn reply_card_raw(
        &self,
        message_id: &str,
        card: serde_json::Value,
    ) -> Result<String> {
        self.client.reply_card_raw(message_id, card).await
    }

    /// Update the body of an existing card with a fully-built card JSON value.
    pub async fn update_card_raw(&self, message_id: &str, card: serde_json::Value) -> Result<()> {
        self.client.update_card_raw(message_id, card).await
    }

    /// Return a [`StreamingCard`] handle that will reply under `parent_msg_id`.
    /// The actual card message is created lazily on the first [`StreamingCard::push`]
    /// or [`StreamingCard::finish`] call, so no placeholder is sent upfront.
    pub fn begin_streaming_reply(&self, parent_msg_id: &str) -> StreamingCard {
        StreamingCard::new(self.client.clone(), parent_msg_id.to_string())
    }

    /// Add an emoji reaction to a message. Returns the `reaction_id` (needed to delete it later).
    pub async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> Result<String> {
        self.client.add_reaction(message_id, emoji_type).await
    }

    /// Remove a previously-added reaction from a message.
    pub async fn delete_reaction(&self, message_id: &str, reaction_id: &str) -> Result<()> {
        self.client.delete_reaction(message_id, reaction_id).await
    }

    /// Download a message resource (image). Returns `(content_type, bytes)`.
    pub async fn download_image(
        &self,
        message_id: &str,
        image_key: &str,
    ) -> Result<(String, Vec<u8>)> {
        self.client.download_image(message_id, image_key).await
    }

    /// Download a file resource from an IM message.
    pub async fn download_file(
        &self,
        message_id: &str,
        file_key: &str,
        file_type: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        self.client
            .download_file(message_id, file_key, file_type)
            .await
    }

    /// Download a Feishu document referenced by URL.
    pub async fn download_document(&self, document_url: &str) -> Result<(String, String, Vec<u8>)> {
        self.client.download_document(document_url).await
    }
    pub async fn download_drive_file(
        &self,
        file_token: &str,
    ) -> anyhow::Result<(String, String, Vec<u8>)> {
        self.client.download_drive_file(file_token).await
    }
    /// Upload a file to Feishu IM and return its file key.
    pub async fn upload_file(
        &self,
        file_name: &str,
        mime_type: &str,
        content: &[u8],
        file_type: &str,
    ) -> Result<String> {
        self.client
            .upload_file(file_name, mime_type, content, file_type)
            .await
    }

    /// Reply to an existing message with a file attachment.
    pub async fn reply_file(
        &self,
        message_id: &str,
        file_key: &str,
        file_type: &str,
    ) -> Result<String> {
        self.client
            .reply_file(message_id, file_key, file_type)
            .await
    }

    /// Send a file attachment into a chat.
    pub async fn send_file(
        &self,
        chat_id: &str,
        file_key: &str,
        file_type: &str,
    ) -> Result<String> {
        self.client.send_file(chat_id, file_key, file_type).await
    }

    /// Return an authenticated Feishu resource URL for a sent file message.
    pub fn file_resource_url(&self, message_id: &str, file_key: &str, file_type: &str) -> String {
        self.client
            .file_resource_url(message_id, file_key, file_type)
    }

    /// Delete (withdraw) a message sent by the bot.
    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        self.client.delete_message(message_id).await
    }

    /// Fetch plain-text content of a message by ID. Returns `None` for unsupported types.
    pub async fn fetch_message_text(&self, message_id: &str) -> Result<Option<String>> {
        let Some((msg_type, content_str)) = self.client.get_message_raw(message_id).await? else {
            return Ok(None);
        };
        let text = match msg_type.as_str() {
            "text" => {
                let tc: TextContent = serde_json::from_str(&content_str)
                    .map_err(|e| anyhow::anyhow!("parse text content: {e}"))?;
                tc.text.trim().to_string()
            }
            "post" => {
                let (text, _, _, _) = extract_from_post(&content_str);
                text
            }
            "interactive" => extract_text_from_card(&content_str),
            _ => return Ok(None),
        };
        Ok(if text.is_empty() { None } else { Some(text) })
    }

    /// Fetch the full content of a quoted/parent message.
    ///
    /// Returns `None` when the message cannot be found or has an unsupported type.
    /// Images are returned as raw keys; callers must download them using this
    /// `message_id` as the resource owner (not the replying message's ID).
    pub async fn fetch_parent_content(&self, message_id: &str) -> Result<Option<ParentContent>> {
        let Some((msg_type, content_str)) = self.client.get_message_raw(message_id).await? else {
            return Ok(None);
        };
        let content = match msg_type.as_str() {
            "text" => {
                let tc: TextContent = serde_json::from_str(&content_str)
                    .map_err(|e| anyhow::anyhow!("parse text content: {e}"))?;
                ParentContent {
                    text: Some(tc.text.trim().to_string()).filter(|s| !s.is_empty()),
                    images: vec![],
                    files: vec![],
                }
            }
            "post" => {
                let (text, images, files, _) = extract_from_post(&content_str);
                ParentContent {
                    text: Some(text).filter(|s| !s.is_empty()),
                    images,
                    files,
                }
            }
            "interactive" => {
                let text = extract_text_from_card(&content_str);
                ParentContent {
                    text: Some(text).filter(|s| !s.is_empty()),
                    images: vec![],
                    files: vec![],
                }
            }
            "image" => {
                let ic: ImageContent = serde_json::from_str(&content_str)
                    .map_err(|e| anyhow::anyhow!("parse image content: {e}"))?;
                ParentContent {
                    text: None,
                    images: vec![ic.image_key],
                    files: vec![],
                }
            }
            "file" | "folder" | "audio" | "media" => {
                let fc: FileContent = serde_json::from_str(&content_str)
                    .map_err(|e| anyhow::anyhow!("parse file content: {e}"))?;
                ParentContent {
                    text: None,
                    images: vec![],
                    files: vec![FeishuFile {
                        file_key: fc.file_key,
                        file_name: fc.file_name,
                        mime_type: fc.mime_type,
                        size_bytes: fc.file_size,
                        file_type: msg_type.to_string(),
                    }],
                }
            }
            _ => return Ok(None),
        };
        Ok(Some(content))
    }
}

// ── Content helpers ───────────────────────────────────────────────────────────

/// Content extracted from a quoted / parent message.
#[derive(Debug, Clone, Default)]
pub struct ParentContent {
    /// Plain-text body, if any.
    pub text: Option<String>,
    /// Raw image keys (must be downloaded using the parent message's ID).
    pub images: Vec<String>,
    /// File attachments.
    pub files: Vec<FeishuFile>,
}

/// Check whether the bot was @-mentioned and strip its placeholder from `text`.
///
/// Feishu uses keys like `@_user_1` in the message text as stand-ins for
/// @-mentions; the `mentions` array maps each key to the actual user/open_id.
///
/// Returns `(cleaned_text, at_bot)` where `at_bot` is `true` if the bot's
/// `open_id` appears in the mentions list.
fn strip_bot_mention(
    text: &str,
    mentions: &[RawMention],
    bot_open_id: Option<&str>,
) -> (String, bool) {
    let Some(bot_id) = bot_open_id else {
        // Bot info not yet fetched — pass text through unchanged, mark as at_bot
        // so we don't silently drop messages.
        return (text.to_string(), true);
    };

    let mut result = text.to_string();
    let mut at_bot = false;

    for mention in mentions {
        if mention.id.open_id.as_deref() == Some(bot_id) {
            result = result.replace(&mention.key, "");
            at_bot = true;
        }
    }

    (result.trim().to_string(), at_bot)
}

/// Extract text content and image keys from a Feishu `post` message body.
fn extract_from_post(
    content_str: &str,
) -> (String, Vec<String>, Vec<FeishuFile>, Vec<FeishuDocument>) {
    let v: serde_json::Value = match serde_json::from_str(content_str) {
        Ok(v) => v,
        Err(_) => return (String::new(), vec![], vec![], vec![]),
    };

    // Feishu `post` content can be either a direct `{title, content}` object
    // or a multilingual wrapper like `{zh_cn: {title, content}}`.
    let lang = if v.get("content").and_then(|c| c.as_array()).is_some() {
        Some(&v)
    } else {
        v.get("zh_cn").or_else(|| {
            v.as_object().and_then(|map| {
                map.values()
                    .find(|value| value.get("content").and_then(|c| c.as_array()).is_some())
            })
        })
    };
    let Some(lang_val) = lang else {
        return (String::new(), vec![], vec![], vec![]);
    };

    let mut texts: Vec<&str> = Vec::new();
    let mut images: Vec<String> = Vec::new();
    let mut documents: Vec<FeishuDocument> = Vec::new();

    if let Some(rows) = lang_val.get("content").and_then(|c| c.as_array()) {
        for row in rows {
            if let Some(items) = row.as_array() {
                for item in items {
                    match item.get("tag").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                                texts.push(t);
                            }
                        }
                        Some("img") => {
                            if let Some(k) = item.get("image_key").and_then(|k| k.as_str()) {
                                images.push(k.to_string());
                            }
                        }
                        _ => {}
                    }
                    collect_doc_links_from_value(item, &mut documents);
                }
            }
        }
    }

    (
        texts.join("").trim().to_string(),
        images,
        vec![],
        unique_documents(documents),
    )
}

/// Extract plain text from a Feishu interactive card content JSON string.
///
/// The card schema used by this bot is:
/// `{"schema":"2.0","body":{"elements":[{"tag":"markdown","content":"..."}]},...}`
///
/// We join the `content` fields of all `markdown` elements, which covers both
/// bot-generated cards and generic Feishu 2.0 cards.
fn extract_text_from_card(content_str: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(content_str) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let mut parts: Vec<&str> = Vec::new();
    if let Some(elements) = v
        .get("body")
        .and_then(|b| b.get("elements"))
        .and_then(|e| e.as_array())
    {
        for el in elements {
            if el.get("tag").and_then(|t| t.as_str()) == Some("markdown") {
                if let Some(text) = el.get("content").and_then(|c| c.as_str()) {
                    parts.push(text);
                }
            }
        }
    }
    parts.join("\n").trim().to_string()
}

fn extract_doc_links_from_text(text: &str) -> Vec<FeishuDocument> {
    unique_documents(
        text.split_whitespace()
            .filter_map(|part| {
                let candidate = part.trim_matches(|c: char| {
                    c.is_ascii_whitespace()
                        || matches!(
                            c,
                            '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
                        )
                });
                parse_feishu_document_url(candidate)
            })
            .collect(),
    )
}

fn merge_documents(
    mut left: Vec<FeishuDocument>,
    right: Vec<FeishuDocument>,
) -> Vec<FeishuDocument> {
    left.extend(right);
    unique_documents(left)
}

fn unique_documents(docs: Vec<FeishuDocument>) -> Vec<FeishuDocument> {
    let mut out: Vec<FeishuDocument> = Vec::new();
    for doc in docs {
        if out.iter().any(|existing| existing.url == doc.url) {
            continue;
        }
        out.push(doc);
    }
    out
}

fn collect_doc_links_from_value(value: &serde_json::Value, docs: &mut Vec<FeishuDocument>) {
    match value {
        serde_json::Value::String(s) => {
            docs.extend(extract_doc_links_from_text(s));
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_doc_links_from_value(item, docs);
            }
        }
        serde_json::Value::Object(map) => {
            let title = map.get("text").and_then(|v| v.as_str()).unwrap_or_default();

            for key in ["href", "link", "url"] {
                if let Some(url) = map.get(key).and_then(|v| v.as_str()) {
                    if let Some(mut doc) = parse_feishu_document_url(url) {
                        if doc.title.is_empty() {
                            doc.title = title.to_string();
                        }
                        docs.push(doc);
                    }
                }
            }

            for nested in map.values() {
                collect_doc_links_from_value(nested, docs);
            }
        }
        _ => {}
    }
}

pub(crate) fn parse_feishu_document_url(raw: &str) -> Option<FeishuDocument> {
    let url = reqwest::Url::parse(raw).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if !host.contains("feishu.cn") && !host.contains("larksuite.com") {
        return None;
    }

    let segments: Vec<&str> = url.path_segments()?.filter(|seg| !seg.is_empty()).collect();
    for (idx, segment) in segments.iter().enumerate() {
        match *segment {
            "drive" => {
                // /drive/file/{token}
                if segments.get(idx + 1).copied() == Some("file") {
                    if let Some(raw_token) = segments.get(idx + 2) {
                        let token = raw_token.split('?').next().unwrap_or("").to_string();
                        if !token.is_empty() {
                            return Some(FeishuDocument {
                                url: raw.to_string(),
                                title: String::new(),
                                doc_type: "file".to_string(),
                                token,
                            });
                        }
                    }
                }
            }
            "docx" | "docs" | "doc" | "wiki" | "sheet" | "sheets" | "base" | "bitable" => {
                let token = match segments.get(idx + 1) {
                    Some(t) => t.split('?').next().unwrap_or("").to_string(),
                    None => return None,
                };
                if token.is_empty() {
                    return None;
                }
                return Some(FeishuDocument {
                    url: raw.to_string(),
                    title: String::new(),
                    doc_type: segment.to_string(),
                    token,
                });
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{extract_doc_links_from_text, extract_from_post, parse_feishu_document_url};

    #[test]
    fn extracts_images_from_direct_post_shape() {
        let payload = r#"{
            "title": "",
            "content": [
                [
                    {"tag": "at", "user_id": "@_user_1", "user_name": "RemiCat-Dev", "style": []},
                    {"tag": "text", "text": " ", "style": []}
                ],
                [
                    {"tag": "img", "image_key": "img_v3_123"}
                ]
            ]
        }"#;

        let (text, images, files, documents) = extract_from_post(payload);
        assert_eq!(text, "");
        assert_eq!(images, vec!["img_v3_123"]);
        assert!(files.is_empty());
        assert!(documents.is_empty());
    }

    #[test]
    fn extracts_images_from_multilingual_post_shape() {
        let payload = r#"{
            "zh_cn": {
                "title": "",
                "content": [
                    [
                        {"tag": "text", "text": "看图", "style": []},
                        {"tag": "img", "image_key": "img_v3_456"}
                    ]
                ]
            }
        }"#;

        let (text, images, files, documents) = extract_from_post(payload);
        assert_eq!(text, "看图");
        assert_eq!(images, vec!["img_v3_456"]);
        assert!(files.is_empty());
        assert!(documents.is_empty());
    }

    #[test]
    fn parses_feishu_docx_url() {
        let doc = parse_feishu_document_url("https://example.feishu.cn/docx/AbCdEf123?from=im")
            .expect("docx url should parse");
        assert_eq!(doc.doc_type, "docx");
        assert_eq!(doc.token, "AbCdEf123");
    }

    #[test]
    fn extracts_doc_links_from_text() {
        let docs = extract_doc_links_from_text(
            "请看 https://foo.feishu.cn/docx/AAA111 和 https://foo.feishu.cn/wiki/BBB222)",
        );
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].token, "AAA111");
        assert_eq!(docs[1].doc_type, "wiki");
    }

    #[test]
    fn parses_feishu_drive_file_url() {
        let doc = parse_feishu_document_url("https://example.feishu.cn/drive/file/XyZ789abc")
            .expect("drive file url should parse");
        assert_eq!(doc.doc_type, "file");
        assert_eq!(doc.token, "XyZ789abc");
    }

    #[test]
    fn parses_feishu_drive_file_url_with_query() {
        let doc =
            parse_feishu_document_url("https://example.feishu.cn/drive/file/XyZ789abc?from=im")
                .expect("drive file url with query should parse");
        assert_eq!(doc.doc_type, "file");
        assert_eq!(doc.token, "XyZ789abc");
    }

    #[test]
    fn file_resource_url_uses_file_type_for_regular_files() {
        use crate::FeishuClient;
        let client = FeishuClient::new(String::from("app_id"), String::from("app_secret"));
        let url = client.file_resource_url("msg_id", "file_key_abc", "file");
        assert!(
            url.ends_with("?type=file"),
            "expected ?type=file, got {url}"
        );
    }

    #[test]
    fn file_resource_url_uses_file_type_for_folders() {
        use crate::FeishuClient;
        let client = FeishuClient::new(String::from("app_id"), String::from("app_secret"));
        let url = client.file_resource_url("msg_id", "file_key_abc", "folder");
        // Feishu IM resource API only accepts image/file — folder must also use type=file
        assert!(
            url.ends_with("?type=file"),
            "expected ?type=file for folder, got {url}"
        );
    }

    #[test]
    fn file_resource_url_uses_image_type_for_images() {
        use crate::FeishuClient;
        let client = FeishuClient::new(String::from("app_id"), String::from("app_secret"));
        let url = client.file_resource_url("msg_id", "img_key_abc", "image");
        assert!(
            url.ends_with("?type=image"),
            "expected ?type=image, got {url}"
        );
    }
}
