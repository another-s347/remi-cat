//! `im-gateway` — Platform-agnostic IM abstraction layer.
//!
//! Defines the `ImGateway` trait and shared event/message types that all
//! IM platform adapters must implement.  The gateway decouples the bot
//! logic from any specific IM platform via an `mpsc` channel for incoming
//! events and async methods for outbound messages.
//!
//! # Event flow
//!
//! ```text
//! IM platform  ──►  ImGateway::start()  ──►  mpsc::Sender<ImEvent>  ──►  bot logic
//! bot logic    ──►  ImGateway::reply_text() / send_text()            ──►  IM platform
//! ```

use thiserror::Error;
use tokio::sync::mpsc;

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("send message failed: {0}")]
    SendFailed(String),

    #[error("connection error: {0}")]
    Connection(String),

    #[error("authentication error: {0}")]
    Auth(String),

    #[error("serialization error: {0}")]
    Serialization(String),
}

pub type GatewayResult<T> = Result<T, GatewayError>;

// ── Events (inbound from IM platform) ─────────────────────────────────────────

/// An event received from the IM platform.
#[derive(Debug, Clone)]
pub enum ImEvent {
    /// A text message was received.
    MessageReceived(ImMessage),

    /// A `/command` message was received (text starting with `/`).
    Command(ImCommand),

    /// The bot was added to a chat / group.
    BotAdded { chat_id: String },

    /// An unrecognised event — carry the raw JSON for debugging.
    Unknown { event_type: String, payload: String },
}

/// A `/command` invocation extracted from an IM message.
#[derive(Debug, Clone)]
pub struct ImCommand {
    /// Platform-specific unique message ID (used for replying).
    pub message_id: String,
    /// Open-platform user ID of the sender.
    pub sender_id: String,
    /// Chat / group ID where the command was sent.
    pub chat_id: String,
    /// Command name without the leading `/` (e.g. `"restart"`, `"status"`).
    pub name: String,
    /// Space-separated arguments following the command name.
    pub args: Vec<String>,
}

impl ImCommand {
    /// Parse a plain-text string into an [`ImCommand`].
    ///
    /// Returns `None` if the text does not start with `/` or contains only `/`.
    pub fn parse(
        message_id: impl Into<String>,
        sender_id: impl Into<String>,
        chat_id: impl Into<String>,
        text: &str,
    ) -> Option<Self> {
        let text = text.trim();
        if !text.starts_with('/') {
            return None;
        }
        let mut parts = text[1..].split_whitespace();
        let name = parts.next()?.to_lowercase();
        if name.is_empty() {
            return None;
        }
        let args = parts.map(str::to_string).collect();
        Some(ImCommand {
            message_id: message_id.into(),
            sender_id: sender_id.into(),
            chat_id: chat_id.into(),
            name,
            args,
        })
    }
}

/// A text message received from an IM user.
#[derive(Debug, Clone)]
pub struct ImMessage {
    /// Platform-specific unique message ID (used for replying).
    pub message_id: String,
    /// Open-platform user ID of the sender.
    pub sender_id: String,
    /// Chat / group ID where the message was sent.
    pub chat_id: String,
    /// Plain-text content of the message.
    pub text: String,
    /// Whether this was a direct (P2P) message or a group message.
    pub is_direct: bool,
}

// ── Gateway trait ─────────────────────────────────────────────────────────────

/// Abstraction over an IM platform connection.
///
/// Implementors connect to the platform, translate incoming events into
/// [`ImEvent`] values forwarded through the provided channel, and expose
/// async methods for sending/replying to messages.
pub trait ImGateway: Send + Sync {
    /// Start receiving events from the platform.
    ///
    /// This method should run until the connection is permanently closed or
    /// an unrecoverable error occurs.  Events are forwarded through `tx`.
    async fn start(&self, tx: mpsc::Sender<ImEvent>) -> GatewayResult<()>;

    /// Send a plain-text message to a chat / group.
    async fn send_text(&self, chat_id: &str, text: &str) -> GatewayResult<()>;

    /// Reply to a specific message by its `message_id`.
    async fn reply_text(&self, message_id: &str, text: &str) -> GatewayResult<()>;
}
