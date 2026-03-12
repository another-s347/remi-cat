//! Streaming card output for Feishu.
//!
//! [`StreamingCard`] wraps a Feishu interactive card message and provides
//! a simple `push(delta)` / `finish()` interface for streaming AI output.
//!
//! Card updates are debounced to [`UPDATE_INTERVAL`] to stay within Feishu's
//! API rate limits.  While streaming, a `▋` cursor is appended so the user
//! can see that output is still in progress.

use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::client::FeishuClient;

/// Minimum interval between successive `PATCH /im/v1/messages` calls.
const UPDATE_INTERVAL: Duration = Duration::from_millis(500);

/// A handle for streaming text into a Feishu interactive card in real-time.
///
/// Obtain one via [`crate::FeishuGateway::begin_streaming_reply`].
/// The card is created lazily on the first [`push`] or [`finish`] call,
/// so no placeholder message is sent before content is ready.
pub struct StreamingCard {
    pub(crate) client: FeishuClient,
    /// The message to reply under — used for deferred card creation.
    parent_msg_id: String,
    /// The `message_id` of the created card, once it exists.
    pub message_id: Option<String>,
    /// Accumulated reply text.
    buffer: String,
    /// Whether [`finish`] has been called.
    done: bool,
    /// Timestamp of the last successful card update.
    last_update: Instant,
}

impl StreamingCard {
    pub(crate) fn new(client: FeishuClient, parent_msg_id: String) -> Self {
        // Subtract UPDATE_INTERVAL * 2 so the first push triggers an update immediately.
        let last_update = Instant::now()
            .checked_sub(UPDATE_INTERVAL * 2)
            .unwrap_or_else(Instant::now);
        Self {
            client,
            parent_msg_id,
            message_id: None,
            buffer: String::new(),
            done: false,
            last_update,
        }
    }

    /// Create the card now if it hasn't been created yet.
    async fn ensure_card(&mut self, initial_text: &str) -> anyhow::Result<()> {
        if self.message_id.is_none() {
            let id = self.client.reply_card(&self.parent_msg_id, initial_text).await?;
            self.message_id = Some(id);
            self.last_update = Instant::now();
        }
        Ok(())
    }

    /// Append `delta` to the buffer, flushing the card if enough time has elapsed.
    pub async fn push(&mut self, delta: &str) -> anyhow::Result<()> {
        self.buffer.push_str(delta);
        if self.last_update.elapsed() >= UPDATE_INTERVAL {
            let content = format!("{}▋", self.buffer);
            if self.message_id.is_none() {
                self.ensure_card(&content).await?;
            } else {
                let id = self.message_id.clone().unwrap();
                match self.client.update_card(&id, &content).await {
                    Ok(()) => {
                        self.last_update = Instant::now();
                        debug!(chars = self.buffer.len(), "streaming card flushed");
                    }
                    Err(e) => {
                        warn!("card update failed (will retry next push): {e:#}");
                    }
                }
            }
        }
        Ok(())
    }

    /// Flush the final buffer (no cursor) and mark the stream as done.
    ///
    /// Safe to call multiple times; subsequent calls are no-ops.
    pub async fn finish(&mut self) -> anyhow::Result<()> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        let text = if self.buffer.is_empty() {
            "（无响应）".to_string()
        } else {
            self.buffer.clone()
        };
        if self.message_id.is_none() {
            // No card yet — create it now with the final content.
            if let Err(e) = self.ensure_card(&text).await {
                warn!("final card creation failed: {e:#}");
            }
        } else {
            let id = self.message_id.clone().unwrap();
            if let Err(e) = self.client.update_card(&id, &text).await {
                warn!("final card update failed: {e:#}");
            }
        }
        Ok(())
    }
}
