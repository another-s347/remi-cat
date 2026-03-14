//! Feishu REST API client.
//!
//! Handles:
//! - `tenant_access_token` acquisition and refresh
//! - Sending/replying messages (text and interactive card)
//! - Updating interactive cards in place (for streaming output)
//! - Fetching the WebSocket endpoint URL for long connections

use anyhow::{anyhow, Context, Result};
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use reqwest::{multipart, Client};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::parse_feishu_document_url;

/// Feishu error code for expired / invalid access token.
const TOKEN_EXPIRED: i64 = 99991663;

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

#[derive(Debug, Deserialize)]
struct ContactUserResponse {
    code: i64,
    msg: String,
    data: Option<ContactUserData>,
}

#[derive(Debug, Deserialize)]
struct ContactUserData {
    user: Option<ContactUser>,
}

#[derive(Debug, Deserialize)]
struct ContactUser {
    name: Option<serde_json::Value>,
    display_name: Option<serde_json::Value>,
    nickname: Option<serde_json::Value>,
    en_name: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContactUserIdType {
    OpenId,
    UserId,
    UnionId,
}

impl ContactUserIdType {
    fn as_str(self) -> &'static str {
        match self {
            Self::OpenId => "open_id",
            Self::UserId => "user_id",
            Self::UnionId => "union_id",
        }
    }
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

    /// Fetch a user's display name from Feishu using an app-scoped user identifier.
    pub async fn get_user_name(&self, user_id: &str) -> Result<Option<String>> {
        let user_id = user_id.trim();
        if user_id.is_empty() {
            return Ok(None);
        }

        let res = self.get_user_name_once(user_id).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!(user_id, "get_user_name: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.get_user_name_once(user_id).await;
        }
        res
    }

    async fn get_user_name_once(&self, user_id: &str) -> Result<Option<String>> {
        let mut last_retryable_err: Option<anyhow::Error> = None;

        for user_id_type in preferred_contact_user_id_types(user_id) {
            match self.get_user_name_for_type(user_id, user_id_type).await {
                Ok(username) => return Ok(username),
                Err(err) if should_retry_contact_user_lookup(&err) => {
                    debug!(
                        user_id,
                        user_id_type = user_id_type.as_str(),
                        error = %err,
                        "get_user_name: retrying with fallback user_id_type"
                    );
                    last_retryable_err = Some(err);
                }
                Err(err) => return Err(err),
            }
        }

        match last_retryable_err {
            Some(err) => Err(err),
            None => Ok(None),
        }
    }

    async fn get_user_name_for_type(
        &self,
        user_id: &str,
        user_id_type: ContactUserIdType,
    ) -> Result<Option<String>> {
        let token = self.token().await;
        let body = self
            .http
            .get(format!("{FEISHU_BASE}/contact/v3/users/{user_id}"))
            .query(&[("user_id_type", user_id_type.as_str())])
            .bearer_auth(&token)
            .send()
            .await
            .with_context(|| format!("get_user_name ({})", user_id_type.as_str()))?
            .text()
            .await
            .context("read get_user_name response body")?;
        let resp: ContactUserResponse = serde_json::from_str(&body)
            .with_context(|| format!("parse get_user_name response body: {body}"))?;

        let api = format!("get_user_name ({})", user_id_type.as_str());
        ensure_ok(resp.code, &resp.msg, &api)?;
        let username = resp
            .data
            .and_then(|data| data.user)
            .and_then(pick_contact_user_display_name);
        match username.as_deref() {
            Some(name) => {
                info!(
                    user_id,
                    user_id_type = user_id_type.as_str(),
                    username = name,
                    "get_user_name: resolved display name"
                );
            }
            None => {
                warn!(
                    user_id,
                    user_id_type = user_id_type.as_str(),
                    "get_user_name: Feishu contact response succeeded but name fields are empty; check contact scopes and app contacts data permissions"
                );
            }
        }
        Ok(username)
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
        let res = self.send_text_once(chat_id, text).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("send_text: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.send_text_once(chat_id, text).await;
        }
        res
    }

    async fn send_text_once(&self, chat_id: &str, text: &str) -> Result<String> {
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
        let res = self.reply_text_once(message_id, text).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("reply_text: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.reply_text_once(message_id, text).await;
        }
        res
    }

    async fn reply_text_once(&self, message_id: &str, text: &str) -> Result<String> {
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

    // ── File messages ─────────────────────────────────────────────────────────

    /// Send a file message to a chat. Returns the new `message_id`.
    pub async fn send_file(&self, chat_id: &str, file_key: &str, file_type: &str) -> Result<String> {
        let res = self.send_file_once(chat_id, file_key, file_type).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("send_file: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.send_file_once(chat_id, file_key, file_type).await;
        }
        res
    }

    async fn send_file_once(&self, chat_id: &str, file_key: &str, file_type: &str) -> Result<String> {
        let msg_type = if file_type == "folder" { "folder" } else { "file" };
        let content = serde_json::json!({ "file_key": file_key }).to_string();
        let token = self.token().await;

        let resp: MessageResponse = self
            .http
            .post(format!(
                "{FEISHU_BASE}/im/v1/messages?receive_id_type=chat_id"
            ))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await
            .context("send_file")?
            .json()
            .await
            .context("parse send_file response")?;

        ensure_ok(resp.code, &resp.msg, "send_file")?;
        debug!("Sent file to chat {chat_id}");
        Ok(message_id_from(resp.data))
    }

    /// Reply to an existing message with a file. Returns the new `message_id`.
    pub async fn reply_file(&self, message_id: &str, file_key: &str, file_type: &str) -> Result<String> {
        let res = self.reply_file_once(message_id, file_key, file_type).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("reply_file: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.reply_file_once(message_id, file_key, file_type).await;
        }
        res
    }

    async fn reply_file_once(&self, message_id: &str, file_key: &str, file_type: &str) -> Result<String> {
        let msg_type = if file_type == "folder" { "folder" } else { "file" };
        let content = serde_json::json!({ "file_key": file_key }).to_string();
        let token = self.token().await;

        let resp: MessageResponse = self
            .http
            .post(format!("{FEISHU_BASE}/im/v1/messages/{message_id}/reply"))
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await
            .context("reply_file")?
            .json()
            .await
            .context("parse reply_file response")?;

        ensure_ok(resp.code, &resp.msg, "reply_file")?;
        debug!("Replied (file) to {message_id}");
        Ok(message_id_from(resp.data))
    }

    /// Upload a file for later IM sending. Returns the Feishu `file_key`.
    pub async fn upload_file(
        &self,
        file_name: &str,
        mime_type: &str,
        content: &[u8],
        file_type: &str,
    ) -> Result<String> {
        let res = self.upload_file_once(file_name, mime_type, content, file_type).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("upload_file: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.upload_file_once(file_name, mime_type, content, file_type).await;
        }
        res
    }

    async fn upload_file_once(
        &self,
        file_name: &str,
        mime_type: &str,
        content: &[u8],
        file_type: &str,
    ) -> Result<String> {
        #[derive(Deserialize)]
        struct UploadData {
            file_key: String,
        }
        #[derive(Deserialize)]
        struct UploadResponse {
            code: i64,
            msg: String,
            data: Option<UploadData>,
        }

        let token = self.token().await;
        let mut part = multipart::Part::bytes(content.to_vec()).file_name(file_name.to_string());
        if !mime_type.trim().is_empty() {
            part = part
                .mime_str(mime_type)
                .context("upload_file invalid mime_type")?;
        }
        let feishu_file_type = if file_type == "folder" { "folder" } else { feishu_file_type_for(mime_type, file_name) };
        let form = multipart::Form::new()
            .text("file_type", feishu_file_type)
            .text("file_name", file_name.to_string())
            .part("file", part);

        let resp: UploadResponse = self
            .http
            .post(format!("{FEISHU_BASE}/im/v1/files"))
            .bearer_auth(&token)
            .multipart(form)
            .send()
            .await
            .context("upload_file")?
            .json()
            .await
            .context("parse upload_file response")?;

        ensure_ok(resp.code, &resp.msg, "upload_file")?;
        let file_key = resp
            .data
            .ok_or_else(|| anyhow!("upload_file missing data"))?
            .file_key;
        debug!(file_name, file_key, "Uploaded file to Feishu");
        Ok(file_key)
    }

    // ── Interactive card messages ─────────────────────────────────────────────

    /// Send an interactive card to a chat. Returns the new `message_id`.
    pub async fn send_card(&self, chat_id: &str, text: &str) -> Result<String> {
        let res = self.send_card_once(chat_id, text).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("send_card: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.send_card_once(chat_id, text).await;
        }
        res
    }

    async fn send_card_once(&self, chat_id: &str, text: &str) -> Result<String> {
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
        let res = self.reply_card_once(message_id, text).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("reply_card: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.reply_card_once(message_id, text).await;
        }
        res
    }

    async fn reply_card_once(&self, message_id: &str, text: &str) -> Result<String> {
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
        let res = self.update_card_once(message_id, text).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("update_card: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.update_card_once(message_id, text).await;
        }
        res
    }

    async fn update_card_once(&self, message_id: &str, text: &str) -> Result<()> {
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
        let res = self.update_card_raw_once(message_id, card.clone()).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("update_card_raw: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.update_card_raw_once(message_id, card).await;
        }
        res
    }

    async fn update_card_raw_once(&self, message_id: &str, card: serde_json::Value) -> Result<()> {
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
        let res = self.reply_card_raw_once(message_id, card.clone()).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("reply_card_raw: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.reply_card_raw_once(message_id, card).await;
        }
        res
    }

    async fn reply_card_raw_once(
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
        let res = self.add_reaction_once(message_id, emoji_type).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("add_reaction: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.add_reaction_once(message_id, emoji_type).await;
        }
        res
    }

    async fn add_reaction_once(&self, message_id: &str, emoji_type: &str) -> Result<String> {
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
        let res = self.delete_reaction_once(message_id, reaction_id).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("delete_reaction: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.delete_reaction_once(message_id, reaction_id).await;
        }
        res
    }

    async fn delete_reaction_once(&self, message_id: &str, reaction_id: &str) -> Result<()> {
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
        let res = self.download_image_once(message_id, image_key).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("download_image: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.download_image_once(message_id, image_key).await;
        }
        res
    }

    async fn download_image_once(
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

        // 401 HTTP status → treat as token expired
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "download_image error {TOKEN_EXPIRED}: token expired"
            ));
        }
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

    /// Download a message resource (file). Returns `(content_type, file_name, bytes)`.
    pub async fn download_file(
        &self,
        message_id: &str,
        file_key: &str,
        file_type: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        let res = self.download_file_once(message_id, file_key, file_type).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("download_file: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.download_file_once(message_id, file_key, file_type).await;
        }
        res
    }

    async fn download_file_once(
        &self,
        message_id: &str,
        file_key: &str,
        file_type: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        let url = self.file_resource_url(message_id, file_key, file_type);
        debug!(message_id, file_key, file_type, url = %url, "download_file_once: requesting");
        let token = self.token().await;
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .context("download_file")?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "download_file error {TOKEN_EXPIRED}: token expired"
            ));
        }
        if !resp.status().is_success() {
            return Err(anyhow!("download_file HTTP {} (url: {})", resp.status(), url));
        }

        let headers = resp.headers().clone();
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let file_name = headers
            .get(CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .and_then(content_disposition_filename)
            .unwrap_or_else(|| format!("{file_key}.bin"));
        let bytes = resp.bytes().await.context("read file bytes")?.to_vec();
        debug!(
            file_key,
            file_name,
            size = bytes.len(),
            "Downloaded file from Feishu"
        );
        Ok((content_type, file_name, bytes))
    }

    /// Download a Feishu document by URL, or a Drive file if the URL is a /drive/file/ link.
    pub async fn download_document(&self, document_url: &str) -> Result<(String, String, Vec<u8>)> {
        let res = self.download_document_once(document_url).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("download_document: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.download_document_once(document_url).await;
        }
        res
    }

    async fn download_document_once(
        &self,
        document_url: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        if let Some(parsed) = parse_feishu_document_url(document_url) {
            if parsed.doc_type == "file" {
                return self.download_drive_file_once(&parsed.token).await;
            }
            if let Some(result) = self.export_document_once(&parsed.doc_type, &parsed.token).await? {
                return Ok(result);
            }
            // doc_type not supported by export API — fall back to raw text
            if let Some(text) = self
                .download_document_raw_text(&parsed.doc_type, &parsed.token)
                .await?
            {
                let file_name = format!(
                    "{}_{}.txt",
                    parsed.doc_type,
                    sanitize_file_name(&parsed.token)
                );
                return Ok(("text/plain".into(), file_name, text.into_bytes()));
            }
        }

        self.download_document_via_url(document_url).await
    }

    /// Export a cloud document via the Feishu Drive export API.
    ///
    /// Returns `Ok(None)` when `doc_type` is not supported by the export API.
    /// Returns `Ok(Some(...))` on success, or `Err` on API / network failure.
    async fn export_document_once(
        &self,
        doc_type: &str,
        token_value: &str,
    ) -> Result<Option<(String, String, Vec<u8>)>> {
        let (export_type, file_extension) = match doc_type {
            "docx" | "doc" | "docs" => ("docx", "docx"),
            "wiki" => ("wiki", "docx"),
            "sheet" | "sheets" => ("sheet", "xlsx"),
            "base" | "bitable" => ("bitable", "xlsx"),
            _ => return Ok(None),
        };

        let bearer = self.token().await;

        // Step 1 – Create export task.
        #[derive(Deserialize)]
        struct ExportTaskData { ticket: String }
        #[derive(Deserialize)]
        struct ExportTaskResp { code: i64, msg: String, data: Option<ExportTaskData> }

        let resp: ExportTaskResp = self
            .http
            .post(format!("{FEISHU_BASE}/drive/v1/export_tasks"))
            .bearer_auth(&bearer)
            .json(&serde_json::json!({
                "file_extension": file_extension,
                "token": token_value,
                "type": export_type,
            }))
            .send()
            .await
            .context("export_document create task")?
            .json()
            .await
            .context("parse export_document create task response")?;

        if resp.code == TOKEN_EXPIRED {
            return Err(anyhow!("export_document error {TOKEN_EXPIRED}: token expired"));
        }
        if resp.code != 0 {
            return Err(anyhow!("export_document create task error {}: {}", resp.code, resp.msg));
        }
        let ticket = resp
            .data
            .ok_or_else(|| anyhow!("export_document missing ticket"))?
            .ticket;

        // Step 2 – Poll until the export job finishes.
        #[derive(Deserialize)]
        struct ExportResult { job_status: i64, file_token: String, file_name: String }
        #[derive(Deserialize)]
        struct ExportResultData { result: ExportResult }
        #[derive(Deserialize)]
        struct ExportPollResp { code: i64, msg: String, data: Option<ExportResultData> }

        let mut file_token = String::new();
        let mut file_name_hint = String::new();
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(800)).await;
            let poll: ExportPollResp = self
                .http
                .get(format!("{FEISHU_BASE}/drive/v1/export_tasks/{ticket}"))
                .query(&[("token", token_value)])
                .bearer_auth(&bearer)
                .send()
                .await
                .context("export_document poll")?
                .json()
                .await
                .context("parse export_document poll response")?;
            if poll.code == TOKEN_EXPIRED {
                return Err(anyhow!("export_document error {TOKEN_EXPIRED}: token expired"));
            }
            if poll.code != 0 {
                return Err(anyhow!("export_document poll error {}: {}", poll.code, poll.msg));
            }
            let result = poll
                .data
                .ok_or_else(|| anyhow!("export_document missing poll data"))?
                .result;
            match result.job_status {
                0 => {
                    file_token = result.file_token;
                    file_name_hint = result.file_name;
                    break;
                }
                3 => return Err(anyhow!("export_document job failed (status 3)")),
                _ => {} // 1=initializing, 2=processing — keep polling
            }
        }
        if file_token.is_empty() {
            return Err(anyhow!("export_document timed out waiting for job"));
        }

        // Step 3 – Download the exported file.
        let dl = self
            .http
            .get(format!("{FEISHU_BASE}/drive/v1/export_tasks/file/{file_token}/download"))
            .bearer_auth(&bearer)
            .send()
            .await
            .context("export_document download")?;

        if dl.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!("export_document error {TOKEN_EXPIRED}: token expired"));
        }
        if !dl.status().is_success() {
            return Err(anyhow!("export_document download HTTP {}", dl.status()));
        }

        let headers = dl.headers().clone();
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let resolved_name = {
            let from_header = headers
                .get(CONTENT_DISPOSITION)
                .and_then(|v| v.to_str().ok())
                .and_then(content_disposition_filename);
            if let Some(n) = from_header {
                n
            } else if !file_name_hint.is_empty() {
                if file_name_hint.contains('.') {
                    file_name_hint
                } else {
                    format!("{file_name_hint}.{file_extension}")
                }
            } else {
                format!("{}_{}.{}", doc_type, sanitize_file_name(token_value), file_extension)
            }
        };
        let bytes = dl.bytes().await.context("read export download bytes")?.to_vec();
        debug!(doc_type, token_value, resolved_name, "Exported cloud document");
        Ok(Some((content_type, resolved_name, bytes)))
    }

    /// Download a file from Feishu Drive by its file_token.
    pub async fn download_drive_file(&self, file_token: &str) -> Result<(String, String, Vec<u8>)> {
        let res = self.download_drive_file_once(file_token).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("download_drive_file: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.download_drive_file_once(file_token).await;
        }
        res
    }

    async fn download_drive_file_once(
        &self,
        file_token: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        let token = self.token().await;
        let resp = self
            .http
            .get(format!(
                "{FEISHU_BASE}/drive/v1/files/{file_token}/download"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .context("download_drive_file")?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "download_drive_file error {TOKEN_EXPIRED}: token expired"
            ));
        }
        if !resp.status().is_success() {
            return Err(anyhow!("download_drive_file HTTP {}", resp.status()));
        }

        let headers = resp.headers().clone();
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let file_name = headers
            .get(CONTENT_DISPOSITION)
            .and_then(|value| value.to_str().ok())
            .and_then(content_disposition_filename)
            .unwrap_or_else(|| {
                let ext = extension_for_content_type(&content_type);
                format!("drive_{file_token}.{ext}")
            });
        let bytes = resp
            .bytes()
            .await
            .context("read drive file bytes")?
            .to_vec();
        debug!(
            file_token,
            file_name,
            size = bytes.len(),
            "Downloaded drive file from Feishu"
        );
        Ok((content_type, file_name, bytes))
    }

    pub fn file_resource_url(&self, message_id: &str, file_key: &str, file_type: &str) -> String {
        // The Feishu IM resource download API only accepts type=image or type=file.
        // Folder attachments are also downloaded with type=file.
        let t = if file_type == "image" { "image" } else { "file" };
        format!("{FEISHU_BASE}/im/v1/messages/{message_id}/resources/{file_key}?type={t}")
    }

    /// Fetch the raw `(msg_type, content_str)` of a message by ID.
    pub async fn get_message_raw(&self, message_id: &str) -> Result<Option<(String, String)>> {
        let res = self.get_message_raw_once(message_id).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("get_message_raw: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.get_message_raw_once(message_id).await;
        }
        res
    }

    async fn get_message_raw_once(&self, message_id: &str) -> Result<Option<(String, String)>> {
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

    /// Delete (withdraw) a message sent by the bot.
    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        let res = self.delete_message_once(message_id).await;
        if res
            .as_ref()
            .err()
            .map(is_token_expired_err)
            .unwrap_or(false)
        {
            warn!("delete_message: token expired, refreshing and retrying");
            self.refresh_token().await?;
            return self.delete_message_once(message_id).await;
        }
        res
    }

    async fn delete_message_once(&self, message_id: &str) -> Result<()> {
        let token = self.token().await;
        let resp: ApiResponse = self
            .http
            .delete(format!("{FEISHU_BASE}/im/v1/messages/{message_id}"))
            .bearer_auth(&token)
            .send()
            .await
            .context("delete_message")?
            .json()
            .await
            .context("parse delete_message response")?;

        ensure_ok(resp.code, &resp.msg, "delete_message")?;
        debug!("Deleted message {message_id}");
        Ok(())
    }

    async fn download_document_raw_text(
        &self,
        doc_type: &str,
        token_value: &str,
    ) -> Result<Option<String>> {
        let endpoints: Vec<String> = match doc_type {
            "docx" | "wiki" => vec![
                format!("{FEISHU_BASE}/docx/v1/documents/{token_value}/raw_content"),
                format!("{FEISHU_BASE}/doc/v2/{token_value}/raw_content"),
            ],
            "doc" | "docs" => vec![
                format!("{FEISHU_BASE}/doc/v2/{token_value}/raw_content"),
                format!("{FEISHU_BASE}/docx/v1/documents/{token_value}/raw_content"),
            ],
            _ => Vec::new(),
        };

        let bearer = self.token().await;
        for endpoint in endpoints {
            let resp = self
                .http
                .get(&endpoint)
                .bearer_auth(&bearer)
                .send()
                .await
                .with_context(|| format!("request document raw content: {endpoint}"))?;

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(anyhow!(
                    "download_document error {TOKEN_EXPIRED}: token expired"
                ));
            }
            if !resp.status().is_success() {
                continue;
            }

            let body: serde_json::Value = resp
                .json()
                .await
                .with_context(|| format!("parse document raw content response: {endpoint}"))?;
            if body.get("code").and_then(|v| v.as_i64()).unwrap_or(0) != 0 {
                continue;
            }
            let data = body.get("data").unwrap_or(&body);
            if let Some(text) = data
                .get("content")
                .or_else(|| data.get("raw_content"))
                .and_then(|v| v.as_str())
            {
                return Ok(Some(text.to_string()));
            }
        }

        Ok(None)
    }

    async fn download_document_via_url(
        &self,
        document_url: &str,
    ) -> Result<(String, String, Vec<u8>)> {
        let token = self.token().await;
        let resp = self
            .http
            .get(document_url)
            .bearer_auth(&token)
            .send()
            .await
            .context("download_document_via_url")?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(anyhow!(
                "download_document error {TOKEN_EXPIRED}: token expired"
            ));
        }
        if !resp.status().is_success() {
            return Err(anyhow!("download_document HTTP {}", resp.status()));
        }

        let headers = resp.headers().clone();
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let bytes = resp.bytes().await.context("read document bytes")?.to_vec();
        let file_name = parse_feishu_document_url(document_url)
            .map(|parsed| {
                let extension = extension_for_content_type(&content_type);
                format!(
                    "{}_{}.{}",
                    parsed.doc_type,
                    sanitize_file_name(&parsed.token),
                    extension
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "feishu_document.{}",
                    extension_for_content_type(&content_type)
                )
            });
        Ok((content_type, file_name, bytes))
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

/// Returns `true` if the error string contains a Feishu token-expired code.
fn is_token_expired_err(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains(&TOKEN_EXPIRED.to_string())
}

fn message_id_from(data: Option<MessageData>) -> String {
    data.map(|d| d.message_id).unwrap_or_default()
}

fn preferred_contact_user_id_types(user_id: &str) -> Vec<ContactUserIdType> {
    let user_id = user_id.trim();
    if user_id.starts_with("ou_") {
        vec![ContactUserIdType::OpenId, ContactUserIdType::UserId]
    } else if user_id.starts_with("on_") {
        vec![
            ContactUserIdType::UnionId,
            ContactUserIdType::OpenId,
            ContactUserIdType::UserId,
        ]
    } else {
        vec![
            ContactUserIdType::UserId,
            ContactUserIdType::OpenId,
            ContactUserIdType::UnionId,
        ]
    }
}

fn should_retry_contact_user_lookup(err: &anyhow::Error) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("41012")
        || text.contains("user id invalid error")
        || text.contains("40001")
        || text.contains("invalid parameter")
}

fn pick_contact_user_display_name(user: ContactUser) -> Option<String> {
    pick_name_fields(
        user.name,
        user.display_name,
        user.nickname,
        user.en_name,
    )
}

fn pick_name_fields(
    name: Option<serde_json::Value>,
    display_name: Option<serde_json::Value>,
    nickname: Option<serde_json::Value>,
    en_name: Option<serde_json::Value>,
) -> Option<String> {
    [name, display_name, nickname, en_name]
        .into_iter()
        .flatten()
        .find_map(extract_employee_name_value)
}

fn extract_employee_name_value(value: serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        serde_json::Value::Array(items) => items.into_iter().find_map(extract_employee_name_value),
        serde_json::Value::Object(map) => {
            for preferred_key in [
                "name",
                "display_name",
                "nickname",
                "en_name",
                "value",
                "text",
                "default_value",
                "localized_value",
            ] {
                if let Some(found) = map
                    .get(preferred_key)
                    .cloned()
                    .and_then(extract_employee_name_value)
                {
                    return Some(found);
                }
            }
            map.into_values().find_map(extract_employee_name_value)
        }
        _ => None,
    }
}

fn content_disposition_filename(value: &str) -> Option<String> {
    for part in value.split(';') {
        let part = part.trim();
        if let Some(name) = part.strip_prefix("filename=") {
            return Some(name.trim_matches('"').to_string());
        }
        if let Some(name) = part.strip_prefix("filename*=UTF-8''") {
            return Some(name.to_string());
        }
    }
    None
}

fn sanitize_file_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "file".to_string()
    } else {
        sanitized
    }
}

fn extension_for_content_type(content_type: &str) -> &'static str {
    match content_type {
        "text/plain" => "txt",
        "text/html" => "html",
        "application/json" => "json",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

/// Map a MIME type + filename to the Feishu IM file upload `file_type` field.
///
/// Feishu accepts: `opus`, `mp4`, `pdf`, `doc`, `xls`, `ppt`, `stream`.
fn feishu_file_type_for(mime_type: &str, file_name: &str) -> &'static str {
    // Check MIME type first, then fall back to extension.
    let mt = mime_type.split(';').next().unwrap_or("").trim();
    match mt {
        "audio/ogg" | "audio/opus" => return "opus",
        "video/mp4" => return "mp4",
        "application/pdf" => return "pdf",
        "application/msword"
        | "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            return "doc"
        }
        "application/vnd.ms-excel"
        | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => return "xls",
        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation" => {
            return "ppt"
        }
        _ => {}
    }
    let ext = std::path::Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "opus" => "opus",
        "mp4" => "mp4",
        "pdf" => "pdf",
        "doc" | "docx" => "doc",
        "xls" | "xlsx" => "xls",
        "ppt" | "pptx" => "ppt",
        _ => "stream",
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::{
        preferred_contact_user_id_types, should_retry_contact_user_lookup, ContactUserIdType,
    };

    #[test]
    fn open_id_prefers_open_id_lookup() {
        assert_eq!(
            preferred_contact_user_id_types("ou_e77520d7b2c16451b00728f176d5b93d"),
            vec![ContactUserIdType::OpenId, ContactUserIdType::UserId]
        );
    }

    #[test]
    fn union_id_prefers_union_id_lookup() {
        assert_eq!(
            preferred_contact_user_id_types("on_4ce7ea0090fe6da686e3c3faedcb459b"),
            vec![
                ContactUserIdType::UnionId,
                ContactUserIdType::OpenId,
                ContactUserIdType::UserId,
            ]
        );
    }

    #[test]
    fn bare_user_id_prefers_user_id_lookup() {
        assert_eq!(
            preferred_contact_user_id_types("5fabb4a1"),
            vec![
                ContactUserIdType::UserId,
                ContactUserIdType::OpenId,
                ContactUserIdType::UnionId,
            ]
        );
    }

    #[test]
    fn invalid_id_errors_trigger_lookup_fallback() {
        let err = anyhow!("get_user_name (open_id) error 41012: user id invalid error");
        assert!(should_retry_contact_user_lookup(&err));
    }

    #[test]
    fn permission_errors_do_not_trigger_lookup_fallback() {
        let err = anyhow!("get_user_name (open_id) error 41050: no user authority");
        assert!(!should_retry_contact_user_lookup(&err));
    }
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
/// Build the secret manager interactive card.
///
/// `keys` is a slice of `(key_name, is_system)` pairs.  System secrets are
/// displayed with a lock indicator and their values are **not** forwarded to
/// the Agent; a small note makes this visible to the admin.
pub fn build_secret_manager_card(keys: &[(&str, bool)]) -> serde_json::Value {
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
        for (key, is_system) in keys {
            let label = if *is_system {
                format!("`{key}` 🔒")
            } else {
                format!("`{key}`")
            };
            let (toggle_text, toggle_action) = if *is_system {
                ("🔓 取消系统", "unmark_system")
            } else {
                ("🔒 设为系统", "mark_system")
            };
            elements.push(serde_json::json!({
                "tag": "column_set",
                "flex_mode": "stretch",
                "columns": [
                    {
                        "tag": "column",
                        "width": "weighted",
                        "weight": 3,
                        "elements": [{
                            "tag": "markdown",
                            "content": label
                        }]
                    },
                    {
                        "tag": "column",
                        "width": "weighted",
                        "weight": 2,
                        "elements": [{
                            "tag": "button",
                            "text": { "tag": "plain_text", "content": toggle_text },
                            "type": "default",
                            "behaviors": [{ "type": "callback", "value": { "action": toggle_action, "key": key } }]
                        }]
                    },
                    {
                        "tag": "column",
                        "width": "weighted",
                        "weight": 1,
                        "elements": [{
                            "tag": "button",
                            "text": { "tag": "plain_text", "content": "🗑️" },
                            "type": "danger",
                            "behaviors": [{ "type": "callback", "value": { "action": "delete", "key": key } }]
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
