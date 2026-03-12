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
//! | 1      | data    | `card`               | ACK, ignore                     |
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
    pub sender_open_id: String,
    pub chat_id: String,
    /// `"p2p"` for direct messages, `"group"` for group chats.
    pub chat_type: String,
    /// Plain-text body (empty for image-only messages).
    /// In group chats, the bot's @-mention placeholder has already been stripped.
    pub text: String,
    /// Image keys for any images attached to the message.
    pub images: Vec<String>,
    /// Parent message ID when this message quotes/replies to another.
    pub parent_id: Option<String>,
    /// `true` when this message @-mentions the bot (always `true` for p2p).
    pub at_bot: bool,
}

/// An emoji reaction added to a message by a user.
#[derive(Debug, Clone)]
pub struct FeishuReaction {
    pub message_id: String,
    pub chat_id: String,
    pub sender_open_id: String,
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

                    if msg_type != TYPE_EVENT {
                        continue; // unknown — ACKed, nothing else to do
                    }

                    let payload = match &frame.payload {
                        Some(p) if !p.is_empty() => p,
                        _ => continue,
                    };

                    if let Some(event) = Self::extract_event(payload, bot_open_id.as_deref()) {
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
                let (raw_text, images) = match msg.message_type.as_str() {
                    "text" => {
                        let tc: TextContent = serde_json::from_str(content_str).ok()?;
                        (tc.text.trim().to_string(), vec![])
                    }
                    "image" => {
                        let ic: ImageContent = serde_json::from_str(content_str).ok()?;
                        (String::new(), vec![ic.image_key])
                    }
                    "post" => extract_from_post(content_str),
                    _ => return None,
                };

                // Determine if the bot was @mentioned, and strip the mention
                // placeholder from the text so the LLM sees clean input.
                let (text, at_bot) = strip_bot_mention(&raw_text, &msg.mentions, bot_open_id);

                // P2P messages always address the bot.
                let at_bot = at_bot || msg.chat_type != "group";

                let sender_id = ev
                    .sender
                    .sender_id
                    .open_id
                    .or(ev.sender.sender_id.user_id)
                    .unwrap_or_default();

                Some(FeishuEvent::MessageReceived(FeishuMessage {
                    message_id: msg.message_id.clone(),
                    sender_open_id: sender_id,
                    chat_id: msg.chat_id.clone(),
                    chat_type: msg.chat_type.clone(),
                    text,
                    images,
                    parent_id: msg.parent_id.clone(),
                    at_bot,
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
                let sender_open_id = event_body
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
                    sender_open_id,
                    emoji_type,
                }))
            }

            "card.action.trigger" => {
                let open_id = event_body.get("open_id")?.as_str()?.to_string();
                let open_message_id = event_body.get("open_message_id")?.as_str()?.to_string();
                let action = event_body.get("action")?;
                let mut action_value = action.get("value")?.clone();
                // Merge form_value fields so callers can read input values directly.
                if let Some(form_value) = action.get("form_value") {
                    if let (Some(av), Some(fv)) =
                        (action_value.as_object_mut(), form_value.as_object())
                    {
                        for (k, val) in fv {
                            av.entry(k).or_insert_with(|| val.clone());
                        }
                    }
                }
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
    ///   "open_id": "ou_...",
    ///   "open_message_id": "om_...",
    ///   "action": { "value": { ... }, "tag": "button" }
    /// }
    /// ```
    fn extract_card_action(payload: &[u8]) -> Option<FeishuEvent> {
        let v: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|e| warn!("card action JSON parse error: {e}"))
            .ok()?;
        let user_open_id = v.get("open_id")?.as_str()?.to_string();
        let card_message_id = v.get("open_message_id")?.as_str()?.to_string();
        let action = v.get("action")?;
        // Merge form_value fields into the button value so callers can read
        // input fields directly from action_value["new_key"] etc.
        let mut action_value = action.get("value")?.clone();
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
                let (text, _) = extract_from_post(&content_str);
                text
            }
            _ => return Ok(None),
        };
        Ok(if text.is_empty() { None } else { Some(text) })
    }
}

// ── Content helpers ───────────────────────────────────────────────────────────

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
fn extract_from_post(content_str: &str) -> (String, Vec<String>) {
    let v: serde_json::Value = match serde_json::from_str(content_str) {
        Ok(v) => v,
        Err(_) => return (String::new(), vec![]),
    };

    // Try zh_cn first, fall back to the first available language key.
    let lang = v
        .get("zh_cn")
        .or_else(|| v.as_object().and_then(|m| m.values().next()));
    let Some(lang_val) = lang else {
        return (String::new(), vec![]);
    };

    let mut texts: Vec<&str> = Vec::new();
    let mut images: Vec<String> = Vec::new();

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
                }
            }
        }
    }

    (texts.join("").trim().to_string(), images)
}
