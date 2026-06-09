use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::acp_bindings::{AcpBindingStore, AcpChannelBinding};
use crate::local_cli::{CliGateway, CliStreamingReply};
use im_feishu::{FeishuGateway, StreamingCard};
use remi_proto::{ImBridgeRequest, ImBridgeResponse, ImDownloadedFile, ImUploadedFile};

const AGENT_FILE_KEY_SEPARATOR: char = '\\';
const LEGACY_AGENT_FILE_KEY_SEPARATOR: char = '\t';

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

pub fn encode_attachment_key(message_id: &str, file_key: &str) -> String {
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

#[derive(Clone)]
pub enum ReplyTransport {
    Feishu(FeishuGateway),
    Cli(CliGateway),
}

impl ReplyTransport {
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<()> {
        match self {
            Self::Feishu(gateway) => {
                gateway.send_text(chat_id, text).await?;
                Ok(())
            }
            Self::Cli(gateway) => gateway.send_text(chat_id, text).await,
        }
    }

    pub async fn reply_text(&self, message_id: &str, chat_id: &str, text: &str) -> Result<()> {
        match self {
            Self::Feishu(gateway) => {
                if let Err(err) = gateway.reply_text(message_id, text).await {
                    gateway.send_text(chat_id, text).await.map_err(|send_err| {
                        anyhow!("reply_text failed: {err:#}; send_text fallback failed: {send_err:#}")
                    })?;
                }
                Ok(())
            }
            Self::Cli(gateway) => gateway.reply_text(message_id, text).await,
        }
    }

    pub async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> Result<Option<String>> {
        match self {
            Self::Feishu(gateway) => gateway.add_reaction(message_id, emoji_type).await.map(Some),
            Self::Cli(_) => Ok(None),
        }
    }

    pub async fn delete_reaction(&self, message_id: &str, reaction_id: &str) -> Result<()> {
        match self {
            Self::Feishu(gateway) => {
                gateway.delete_reaction(message_id, reaction_id).await?;
                Ok(())
            }
            Self::Cli(_) => Ok(()),
        }
    }

    pub async fn reply_card(&self, message_id: &str, markdown: &str) -> Result<()> {
        match self {
            Self::Feishu(gateway) => {
                gateway.reply_card(message_id, markdown).await?;
                Ok(())
            }
            Self::Cli(gateway) => gateway.reply_card(message_id, markdown).await,
        }
    }

    pub async fn reply_card_raw(&self, message_id: &str, card: serde_json::Value) -> Result<()> {
        match self {
            Self::Feishu(gateway) => {
                gateway.reply_card_raw(message_id, card).await?;
                Ok(())
            }
            Self::Cli(gateway) => {
                gateway
                    .reply_card(message_id, &serde_json::to_string_pretty(&card)?)
                    .await
            }
        }
    }

    pub fn begin_streaming_reply(&self, reply_to: &str) -> StreamingReply {
        match self {
            Self::Feishu(gateway) => StreamingReply::Feishu(gateway.begin_streaming_reply(reply_to)),
            Self::Cli(gateway) => StreamingReply::Cli(gateway.begin_streaming_reply(reply_to)),
        }
    }

    pub async fn get_user_name(&self, user_id: &str) -> Result<Option<String>> {
        match self {
            Self::Feishu(gateway) => gateway.get_user_name(user_id).await,
            Self::Cli(_) => Ok(Some(user_id.to_string())),
        }
    }
}

pub enum StreamingReply {
    Feishu(StreamingCard),
    Cli(CliStreamingReply),
}

impl StreamingReply {
    pub async fn push(&mut self, text: &str) -> Result<()> {
        match self {
            Self::Feishu(card) => {
                card.push(text).await?;
                Ok(())
            }
            Self::Cli(card) => card.push(text).await,
        }
    }

    pub async fn finish(&mut self) -> Result<()> {
        match self {
            Self::Feishu(card) => {
                card.finish().await?;
                Ok(())
            }
            Self::Cli(card) => card.finish().await,
        }
    }
}

pub async fn handle_im_bridge_request(
    transport: &ReplyTransport,
    acp_bindings: &Arc<Mutex<AcpBindingStore>>,
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
            let ReplyTransport::Feishu(gateway) = transport else {
                return ImBridgeResponse {
                    request_id,
                    error: "local cli transport does not support file download".into(),
                    payload: None,
                };
            };
            if download.platform != "feishu" {
                return ImBridgeResponse {
                    request_id,
                    error: format!("unsupported platform: {}", download.platform),
                    payload: None,
                };
            }
            let result = if !download.attachment_key.is_empty() {
                let (owner_msg_id, real_key) = decode_agent_file_key(&download.attachment_key)
                    .map(|(owner, key)| (owner.to_string(), key.to_string()))
                    .unwrap_or_else(|| {
                        (download.message_id.clone(), download.attachment_key.clone())
                    });
                gateway
                    .download_file(&owner_msg_id, &real_key, &download.file_type)
                    .await
                    .map(|(mime_type, file_name, content)| ImDownloadedFile {
                        file_name,
                        mime_type,
                        content,
                        source_label: format!("attachment:{real_key}"),
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
                Err(anyhow!("download request must specify attachment_key or document_url"))
            };
            match result {
                Ok(downloaded) => ImBridgeResponse {
                    request_id,
                    error: String::new(),
                    payload: Some(remi_proto::im_bridge_response::Payload::Downloaded(downloaded)),
                },
                Err(err) => ImBridgeResponse {
                    request_id,
                    error: err.to_string(),
                    payload: None,
                },
            }
        }
        remi_proto::im_bridge_request::Payload::Upload(upload) => {
            let ReplyTransport::Feishu(gateway) = transport else {
                return ImBridgeResponse {
                    request_id,
                    error: "local cli transport does not support file upload".into(),
                    payload: None,
                };
            };
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
                        Err(_) => gateway
                            .send_file(&upload.chat_id, &file_key, &upload.file_type)
                            .await?,
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
                Err(err) => ImBridgeResponse {
                    request_id,
                    error: err.to_string(),
                    payload: None,
                },
            }
        }
        remi_proto::im_bridge_request::Payload::AcpBindingUpsert(binding) => {
            let result = acp_bindings.lock().await.upsert(AcpChannelBinding {
                session_id: binding.session_id,
                platform: binding.platform,
                channel_id: binding.channel_id,
                updated_at: chrono::Utc::now().to_rfc3339(),
            });
            match result {
                Ok(()) => ImBridgeResponse {
                    request_id,
                    error: String::new(),
                    payload: None,
                },
                Err(err) => ImBridgeResponse {
                    request_id,
                    error: err.to_string(),
                    payload: None,
                },
            }
        }
        remi_proto::im_bridge_request::Payload::AcpBindingDelete(binding) => {
            let result = acp_bindings
                .lock()
                .await
                .delete(&binding.platform, &binding.channel_id);
            match result {
                Ok(_) => ImBridgeResponse {
                    request_id,
                    error: String::new(),
                    payload: None,
                },
                Err(err) => ImBridgeResponse {
                    request_id,
                    error: err.to_string(),
                    payload: None,
                },
            }
        }
    }
}
