use std::collections::HashMap;

use anyhow::Context;
use bot_core::im_tools::{
    decode_agent_file_key, AcpBindingDeleteRequest, AcpBindingUpsertRequest, BoundImChannel,
    DownloadedImFile, ImDownloadRequest, ImFileBridge, ImUploadRequest,
    SubSessionBindingUpsertRequest, UploadedImFile,
};
use im_feishu::{FeishuGateway, StreamingCard};
use tokio::sync::Mutex;

use super::feishu_topic_channel_id;
use crate::app::FEISHU_CHANNEL;

pub(crate) struct LocalImFileBridge {
    gateway: Option<FeishuGateway>,
    sub_session_cards: Mutex<HashMap<String, StreamingCard>>,
}

impl LocalImFileBridge {
    pub(crate) fn new(gateway: Option<FeishuGateway>) -> Self {
        Self {
            gateway,
            sub_session_cards: Mutex::new(HashMap::new()),
        }
    }
}

impl ImFileBridge for LocalImFileBridge {
    fn download<'a>(
        &'a self,
        req: ImDownloadRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<DownloadedImFile>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(gateway) = &self.gateway else {
                anyhow::bail!("current transport does not support IM file download");
            };
            if req.platform != FEISHU_CHANNEL {
                anyhow::bail!("unsupported platform: {}", req.platform);
            }
            let (mime_type, file_name, content, source_label) =
                if let Some(key) = req.attachment_key.filter(|k| !k.is_empty()) {
                    let decoded = decode_agent_file_key(&key);
                    let owner_message_id = decoded
                        .as_ref()
                        .map(|value| value.message_id.as_str())
                        .filter(|value| !value.is_empty())
                        .unwrap_or(req.message_id.as_str());
                    let real_key = decoded
                        .as_ref()
                        .map(|value| value.file_key.as_str())
                        .unwrap_or(key.as_str());
                    let (mt, fn_, c) = gateway
                        .download_file(owner_message_id, real_key, &req.file_type)
                        .await?;
                    (mt, fn_, c, format!("attachment:{real_key}"))
                } else if let Some(url) = req.document_url.filter(|u| !u.is_empty()) {
                    let (mt, fn_, c) = gateway.download_document(&url).await?;
                    (mt, fn_, c, url)
                } else {
                    anyhow::bail!("download request must specify attachment_key or document_url");
                };
            Ok(DownloadedImFile {
                file_name,
                mime_type,
                content,
                source_label,
            })
        })
    }

    fn upload<'a>(
        &'a self,
        req: ImUploadRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<UploadedImFile>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(gateway) = &self.gateway else {
                anyhow::bail!("current transport does not support IM file upload");
            };
            if req.platform != FEISHU_CHANNEL {
                anyhow::bail!("unsupported platform: {}", req.platform);
            }
            let file_key = gateway
                .upload_file(&req.file_name, &req.mime_type, &req.content, &req.file_type)
                .await?;
            let sent_message_id = if !req.message_id.is_empty() {
                match gateway
                    .reply_file(&req.message_id, &file_key, &req.file_type)
                    .await
                {
                    Ok(message_id) => message_id,
                    Err(_) => {
                        gateway
                            .send_file(&req.chat_id, &file_key, &req.file_type)
                            .await?
                    }
                }
            } else {
                gateway
                    .send_file(&req.chat_id, &file_key, &req.file_type)
                    .await?
            };
            Ok(UploadedImFile {
                file_name: req.file_name,
                file_key: file_key.clone(),
                message_id: sent_message_id.clone(),
                resource_url: gateway.file_resource_url(
                    &sent_message_id,
                    &file_key,
                    &req.file_type,
                ),
            })
        })
    }

    fn acp_binding_upsert<'a>(
        &'a self,
        _request: AcpBindingUpsertRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move { Ok(()) })
    }

    fn acp_binding_delete<'a>(
        &'a self,
        _request: AcpBindingDeleteRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move { Ok(()) })
    }

    fn sub_session_binding_upsert<'a>(
        &'a self,
        request: SubSessionBindingUpsertRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<Option<BoundImChannel>>> + Send + 'a>,
    > {
        Box::pin(async move {
            if request.platform != FEISHU_CHANNEL {
                return Ok(None);
            }
            let Some(gateway) = &self.gateway else {
                return Ok(None);
            };
            let title = request
                .title
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("{} {}", request.target, request.sub_session_id));
            let chat_name = format!("remi {}: {}", request.kind, title);
            if request.kind == "fork"
                && request
                    .parent_thread_id
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
            {
                let topic_text = format!(
                    "Fork session `{}` is bound here.\nparent_session_id: {}\ntarget: {}",
                    request.sub_session_id, request.parent_session_id, request.target
                );
                let (_, thread_id) = gateway
                    .send_topic_text(&request.parent_channel_id, &topic_text)
                    .await
                    .with_context(|| {
                        format!(
                            "POST /open-apis/im/v1/messages failed while binding fork topic; parent_chat_id={}, parent_thread_id={:?}, sub_session_id={}",
                            request.parent_channel_id,
                            request.parent_thread_id,
                            request.sub_session_id,
                        )
                    })?;
                return Ok(Some(BoundImChannel {
                    platform: FEISHU_CHANNEL.to_string(),
                    channel_id: feishu_topic_channel_id(&request.parent_channel_id, &thread_id),
                }));
            }
            let chat_id = gateway
                .create_sub_session_chat(
                    &chat_name,
                    &request.parent_channel_id,
                    request.actor_user_id.as_deref(),
                )
                .await
                .with_context(|| {
                    format!(
                        "POST /open-apis/im/v1/chats?user_id_type=open_id failed while binding sub-session IM channel; owner_id/user_id_list={:?}, bot_id_list=[app_id], parent_chat_id={}, sub_session_id={}, kind={}",
                        request.actor_user_id,
                        request.parent_channel_id,
                        request.sub_session_id,
                        request.kind
                    )
                })?;
            let _ = gateway
                .send_text(
                    &chat_id,
                    &format!(
                        "Sub-session `{}` ({}) is bound here.\nparent_session_id: {}\ntarget: {}",
                        request.sub_session_id,
                        request.kind,
                        request.parent_session_id,
                        request.target
                    ),
                )
                .await;
            Ok(Some(BoundImChannel {
                platform: FEISHU_CHANNEL.to_string(),
                channel_id: chat_id,
            }))
        })
    }

    fn sub_session_send_text<'a>(
        &'a self,
        platform: &'a str,
        channel_id: &'a str,
        text: &'a str,
        done: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if platform != FEISHU_CHANNEL {
                return Ok(());
            }
            let Some(gateway) = &self.gateway else {
                return Ok(());
            };
            let mut cards = self.sub_session_cards.lock().await;
            let card = cards
                .entry(channel_id.to_string())
                .or_insert_with(|| gateway.begin_streaming_reply(channel_id));
            if done {
                card.replace_final(text).await?;
                cards.remove(channel_id);
            } else {
                card.replace(text).await?;
            }
            Ok(())
        })
    }
}
