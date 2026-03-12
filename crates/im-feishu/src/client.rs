//! Feishu REST API client.
//!
//! Handles:
//! - `tenant_access_token` acquisition and refresh
//! - Sending/replying messages (text and interactive card)
//! - Updating interactive cards in place (for streaming output)
//! - Fetching the WebSocket endpoint URL for long connections

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

const FEISHU_BASE: &str = "https://open.feishu.cn/open-apis";
/// Base URL for the WebSocket endpoint (NOT under /open-apis/).
const FEISHU_DOMAIN: &str = "https://open.feishu.cn";

// ── Token ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TokenResponse {
    code: i64,
    msg: String,
    tenant_access_token: Option<String>,
}

// ── WS endpoint ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct WsEndpointResponse {
    code: i64,
    msg: String,
    data: Option<WsEndpointData>,
}

#[derive(Debug, Deserialize)]
pub struct WsEndpointData {
    #[serde(rename = "URL")]
    pub url: String,
    #[serde(rename = "ClientID")]
    pub client_id: Option<String>,
}

// ── Message response ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct MessageResponse {
    code: i64,
    msg: String,
    data: Option<MessageData>,
}

#[derive(Debug, Deserialize)]
struct MessageData {
    message_id: String,
}

// ── Generic API response ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ApiResponse {
    code: i64,
    msg: String,
}

// ── FeishuClient ──────────────────────────────────────────────────────────────

/// Low-level Feishu REST client with auto-refreshing `tenant_access_token`.
///
/// Cheaply cloneable — all state (including the token) is shared via `Arc`.
#[derive(Clone)]
pub struct FeishuClient {
    app_id: String,
    app_secret: String,
    http: Client,
    token: Arc<RwLock<String>>,
    /// Bot's own `open_id`, fetched once via `/open-apis/bot/v3/info`.
    bot_open_id: Arc<RwLock<Option<String>>>,
}

impl FeishuClient {
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            http: Client::new(),
            token: Arc::new(RwLock::new(String::new())),
            bot_open_id: Arc::new(RwLock::new(None)),
        }
    }

    /// Fetch a fresh `tenant_access_token` and store it internally.
    pub async fn refresh_token(&self) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            app_id: &'a str,
            app_secret: &'a str,
        }

        let resp: TokenResponse = self
            .http
            .post(format!(
                "{FEISHU_BASE}/auth/v3/tenant_access_token/internal"
            ))
            .json(&Body {
                app_id: &self.app_id,
                app_secret: &self.app_secret,
            })
            .send()
            .await
            .context("request tenant_access_token")?
            .json()
            .await
            .context("parse token response")?;

        if resp.code != 0 {
            return Err(anyhow!(
                "tenant_access_token error {}: {}",
                resp.code,
                resp.msg
            ));
        }

        let token = resp
            .tenant_access_token
            .ok_or_else(|| anyhow!("missing tenant_access_token in response"))?;

        info!("Feishu token refreshed");
        *self.token.write().await = token;
        Ok(())
    }

    /// Return the current bearer token string.
    pub async fn token(&self) -> String {
        self.token.read().await.clone()
    }

    /// Fetch the bot's own `open_id` via `/open-apis/bot/v3/info` and store it.
    /// Must be called after `refresh_token()`.
    pub async fn init_bot_info(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct BotInfoResp {
            code: i64,
            msg: String,
            bot: Option<BotData>,
        }
        #[derive(Deserialize)]
        struct BotData {
            open_id: String,
        }

        let token = self.token().await;
        let resp: BotInfoResp = self
            .http
            .get(format!("{FEISHU_BASE}/bot/v3/info"))
            .bearer_auth(&token)
            .send()
            .await
            .context("request bot info")?
            .json()
            .await
            .context("parse bot info response")?;

        if resp.code != 0 {
            return Err(anyhow!("bot info error {}: {}", resp.code, resp.msg));
        }
        let open_id = resp
            .bot
            .ok_or_else(|| anyhow!("missing bot field in bot info response"))?
            .open_id;
        info!(open_id = %open_id, "bot info fetched");
        *self.bot_open_id.write().await = Some(open_id);
        Ok(())
    }

    /// Return the bot's own `open_id`, or `None` if not yet initialised.
    pub async fn get_bot_open_id(&self) -> Option<String> {
        self.bot_open_id.read().await.clone()
    }

    /// Get the WebSocket long-connection endpoint from Feishu.
    ///
    /// Per the official SDK: auth is passed as `AppID`/`AppSecret` in the
    /// request body — **not** as a bearer token.
    pub async fn ws_endpoint(&self) -> Result<WsEndpointData> {
        let http_resp = self
            .http
            .post(format!("{FEISHU_DOMAIN}/callback/ws/endpoint"))
            .header("locale", "zh")
            .json(&serde_json::json!({
                "AppID":     self.app_id,
                "AppSecret": self.app_secret,
            }))
            .send()
            .await
            .context("request ws endpoint")?;

        let status = http_resp.status();
        let body = http_resp.bytes().await.context("read ws endpoint body")?;

        if !status.is_success() {
            let body_text = String::from_utf8_lossy(&body);
            return Err(anyhow!("ws endpoint HTTP {status}: {body_text}"));
        }

        let resp: WsEndpointResponse =
            serde_json::from_slice(&body).context("parse ws endpoint response")?;

        if resp.code != 0 {
            return Err(anyhow!("ws endpoint error {}: {}", resp.code, resp.msg));
        }

        resp.data
            .ok_or_else(|| anyhow!("missing data in ws endpoint response"))
    }

    // ── Text messages ─────────────────────────────────────────────────────────

    /// Send a plain-text message to a chat. Returns the new `message_id`.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String> {
        let content = serde_json::json!({ "text": text }).to_string();
        let token = self.token().await;

        let resp: MessageResponse = self
            .http
            .post(format!(
                "{FEISHU_BASE}/im/v1/messages?receive_id_type=chat_id"
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": "text",
                "content": content,
            }))
            .send()
            .await
            .context("send_text")?
            .json()
            .await
            .context("parse send_text response")?;

        ensure_ok(resp.code, &resp.msg, "send_text")?;
        debug!("Sent text to chat {chat_id}");
        Ok(message_id_from(resp.data))
    }

    /// Reply to an existing message with plain text. Returns the new `message_id`.
    pub async fn reply_text(&self, message_id: &str, text: &str) -> Result<String> {
        let content = serde_json::json!({ "text": text }).to_string();
        let token = self.token().await;

        let resp: MessageResponse = self
            .http
            .post(format!("{FEISHU_BASE}/im/v1/messages/{message_id}/reply"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "msg_type": "text",
                "content": content,
            }))
            .send()
            .await
            .context("reply_text")?
            .json()
            .await
            .context("parse reply_text response")?;

        ensure_ok(resp.code, &resp.msg, "reply_text")?;
        debug!("Replied (text) to {message_id}");
        Ok(message_id_from(resp.data))
    }

    // ── Interactive card messages ─────────────────────────────────────────────

    /// Send an interactive card to a chat. Returns the new `message_id`.
    pub async fn send_card(&self, chat_id: &str, text: &str) -> Result<String> {
        let content = card_content(text);
        let token = self.token().await;

        let resp: MessageResponse = self
            .http
            .post(format!(
                "{FEISHU_BASE}/im/v1/messages?receive_id_type=chat_id"
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": "interactive",
                "content": content,
            }))
            .send()
            .await
            .context("send_card")?
            .json()
            .await
            .context("parse send_card response")?;

        ensure_ok(resp.code, &resp.msg, "send_card")?;
        debug!("Sent card to chat {chat_id}");
        Ok(message_id_from(resp.data))
    }

    /// Reply to an existing message with an interactive card.
    /// Returns the new card `message_id` (used for subsequent [`update_card`] calls).
    pub async fn reply_card(&self, message_id: &str, text: &str) -> Result<String> {
        let content = card_content(text);
        let token = self.token().await;

        let resp: MessageResponse = self
            .http
            .post(format!("{FEISHU_BASE}/im/v1/messages/{message_id}/reply"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "msg_type": "interactive",
                "content": content,
            }))
            .send()
            .await
            .context("reply_card")?
            .json()
            .await
            .context("parse reply_card response")?;

        ensure_ok(resp.code, &resp.msg, "reply_card")?;
        debug!("Replied (card) to {message_id}");
        Ok(message_id_from(resp.data))
    }

    /// Update the body of an existing interactive card.
    ///
    /// Used for streaming: patch the card's content as new text arrives.
    /// Feishu rate-limits card updates; callers should debounce (see [`StreamingCard`]).
    pub async fn update_card(&self, message_id: &str, text: &str) -> Result<()> {
        let content = card_content(text);
        let token = self.token().await;

        let resp: ApiResponse = self
            .http
            .patch(format!("{FEISHU_BASE}/im/v1/messages/{message_id}"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await
            .context("update_card")?
            .json()
            .await
            .context("parse update_card response")?;

        ensure_ok(resp.code, &resp.msg, "update_card")?;
        debug!("Updated card {message_id}");
        Ok(())
    }

    /// Update an existing interactive card with a fully-built card JSON value.
    pub async fn update_card_raw(&self, message_id: &str, card: serde_json::Value) -> Result<()> {
        let content = card.to_string();
        let token = self.token().await;

        let resp: ApiResponse = self
            .http
            .patch(format!("{FEISHU_BASE}/im/v1/messages/{message_id}"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "content": content }))
            .send()
            .await
            .context("update_card_raw")?
            .json()
            .await
            .context("parse update_card_raw response")?;

        ensure_ok(resp.code, &resp.msg, "update_card_raw")?;
        debug!("Updated card (raw) {message_id}");
        Ok(())
    }

    /// Reply to a message with a fully-built card JSON value.
    /// Returns the new card `message_id`.
    pub async fn reply_card_raw(
        &self,
        message_id: &str,
        card: serde_json::Value,
    ) -> Result<String> {
        let content = card.to_string();
        let token = self.token().await;

        let resp: MessageResponse = self
            .http
            .post(format!("{FEISHU_BASE}/im/v1/messages/{message_id}/reply"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "msg_type": "interactive",
                "content": content,
            }))
            .send()
            .await
            .context("reply_card_raw")?
            .json()
            .await
            .context("parse reply_card_raw response")?;

        ensure_ok(resp.code, &resp.msg, "reply_card_raw")?;
        debug!("Replied (raw card) to {message_id}");
        Ok(message_id_from(resp.data))
    }

    // ── Reactions ─────────────────────────────────────────────────────────────

    /// Add an emoji reaction to a message. Returns the `reaction_id`.
    pub async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct ReactionData {
            reaction_id: String,
        }
        #[derive(Deserialize)]
        struct ReactionResp {
            code: i64,
            msg: String,
            data: Option<ReactionData>,
        }

        let token = self.token().await;
        let resp: ReactionResp = self
            .http
            .post(format!(
                "{FEISHU_BASE}/im/v1/messages/{message_id}/reactions"
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "reaction_type": { "emoji_type": emoji_type } }))
            .send()
            .await
            .context("add_reaction")?
            .json()
            .await
            .context("parse add_reaction response")?;

        ensure_ok(resp.code, &resp.msg, "add_reaction")?;
        debug!("Added reaction {emoji_type} to {message_id}");
        Ok(resp.data.map(|d| d.reaction_id).unwrap_or_default())
    }

    /// Remove a reaction from a message.
    pub async fn delete_reaction(&self, message_id: &str, reaction_id: &str) -> Result<()> {
        let token = self.token().await;
        let resp: ApiResponse = self
            .http
            .delete(format!(
                "{FEISHU_BASE}/im/v1/messages/{message_id}/reactions/{reaction_id}"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .context("delete_reaction")?
            .json()
            .await
            .context("parse delete_reaction response")?;

        ensure_ok(resp.code, &resp.msg, "delete_reaction")?;
        debug!("Deleted reaction {reaction_id} from {message_id}");
        Ok(())
    }

    /// Download a message resource (image). Returns `(content_type, bytes)`.
    pub async fn download_image(
        &self,
        message_id: &str,
        image_key: &str,
    ) -> Result<(String, Vec<u8>)> {
        let token = self.token().await;
        let resp = self
            .http
            .get(format!(
                "{FEISHU_BASE}/im/v1/messages/{message_id}/resources/{image_key}?type=image"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .context("download_image")?;

        if !resp.status().is_success() {
            return Err(anyhow!("download_image HTTP {}", resp.status()));
        }

        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "image/jpeg".to_string());
        let bytes = resp.bytes().await.context("read image bytes")?.to_vec();
        debug!("Downloaded image {image_key} ({} bytes)", bytes.len());
        Ok((content_type, bytes))
    }

    /// Fetch the raw `(msg_type, content_str)` of a message by ID.
    pub async fn get_message_raw(&self, message_id: &str) -> Result<Option<(String, String)>> {
        #[derive(Deserialize)]
        struct GetResp {
            code: i64,
            msg: String,
            data: Option<GetData>,
        }
        #[derive(Deserialize)]
        struct GetData {
            items: Vec<GetItem>,
        }
        #[derive(Deserialize)]
        struct GetItem {
            msg_type: String,
            body: GetBody,
        }
        #[derive(Deserialize)]
        struct GetBody {
            content: String,
        }

        let token = self.token().await;
        let resp: GetResp = self
            .http
            .get(format!("{FEISHU_BASE}/im/v1/messages/{message_id}"))
            .bearer_auth(&token)
            .send()
            .await
            .context("get_message")?
            .json()
            .await
            .context("parse get_message response")?;

        ensure_ok(resp.code, &resp.msg, "get_message")?;
        let Some(data) = resp.data else {
            return Ok(None);
        };
        let Some(item) = data.items.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some((item.msg_type, item.body.content)))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a Feishu schema-2.0 interactive card JSON string from plain text.
///
/// The returned `String` is used directly as the `"content"` field value in
/// Feishu message APIs — Feishu expects the card to be a serialised JSON string,
/// not a nested object.
pub fn card_content(text: &str) -> String {
    serde_json::json!({
        "schema": "2.0",
        "body": {
            "elements": [{
                "tag": "markdown",
                "content": text
            }]
        },
        "header": {
            "title": { "tag": "plain_text", "content": "remi-cat" },
            "template": "blue"
        }
    })
    .to_string()
}

fn ensure_ok(code: i64, msg: &str, api: &str) -> Result<()> {
    if code != 0 {
        Err(anyhow!("{api} error {code}: {msg}"))
    } else {
        Ok(())
    }
}

fn message_id_from(data: Option<MessageData>) -> String {
    data.map(|d| d.message_id).unwrap_or_default()
}

// ── Secret manager card ───────────────────────────────────────────────────────

/// Build a Feishu Schema 2.0 **interactive** card for the secret manager.
///
/// The card lists every existing key with a 🗑️ delete button, and provides
/// a two-field form (Key + Value) for adding/updating a secret.
///
/// # Button / form action values
///
/// Delete button: `{ "action": "delete", "key": "<KEY>" }`
/// Set form:      `{ "action": "set" }` — the form `input_confirm` callback
///                carries the field values in `action.form_value`:
///                `{ "new_key": "<KEY>", "new_value": "<VALUE>" }`.
pub fn build_secret_manager_card(keys: &[&str]) -> serde_json::Value {
    let mut elements: Vec<serde_json::Value> = Vec::new();

    if keys.is_empty() {
        elements.push(serde_json::json!({
            "tag": "markdown",
            "content": "_（暂无 Secret）_"
        }));
    } else {
        elements.push(serde_json::json!({
            "tag": "markdown",
            "content": "**已配置的 Secrets：**"
        }));
        for key in keys {
            elements.push(serde_json::json!({
                "tag": "column_set",
                "flex_mode": "stretch",
                "columns": [
                    {
                        "tag": "column",
                        "width": "weighted",
                        "weight": 4,
                        "elements": [{
                            "tag": "markdown",
                            "content": format!("`{key}`")
                        }]
                    },
                    {
                        "tag": "column",
                        "width": "weighted",
                        "weight": 1,
                        "elements": [{
                            "tag": "button",
                            "text": { "tag": "plain_text", "content": "🗑️ 删除" },
                            "type": "danger",
                            "value": { "action": "delete", "key": key }
                        }]
                    }
                ]
            }));
        }
    }

    // Divider
    elements.push(serde_json::json!({ "tag": "hr" }));

    // Add / update form — inputs must live inside a `form` container so that
    // Feishu bundles the field values into `action.form_value` on callback.
    elements.push(serde_json::json!({
        "tag": "form",
        "name": "secret_form",
        "elements": [
            {
                "tag": "markdown",
                "content": "**添加 / 更新 Secret：**"
            },
            {
                "tag": "input",
                "name": "new_key",
                "label": { "tag": "plain_text", "content": "Key（环境变量名）" },
                "placeholder": { "tag": "plain_text", "content": "例如 GITHUB_TOKEN" }
            },
            {
                "tag": "input",
                "name": "new_value",
                "label": { "tag": "plain_text", "content": "Value" },
                "placeholder": { "tag": "plain_text", "content": "值（不可为空）" }
            },
            {
                "tag": "button",
                "name": "submit_btn",
                "form_action_type": "submit",
                "text": { "tag": "plain_text", "content": "✅ 设置" },
                "type": "primary",
                "behaviors": [{ "type": "callback", "value": { "action": "set" } }]
            }
        ]
    }));

    serde_json::json!({
        "schema": "2.0",
        "body": { "elements": elements },
        "header": {
            "title": { "tag": "plain_text", "content": "🔑 Secret 管理" },
            "template": "yellow"
        }
    })
}
