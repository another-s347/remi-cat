use std::collections::HashMap;

use anyhow::Context;
use bot_core::im_tools::{
    decode_agent_file_key, AcpBindingDeleteRequest, AcpBindingUpsertRequest, BoundImChannel,
    DownloadedImFile, ImDownloadRequest, ImFileBridge, ImUploadRequest,
    SubSessionBindingUpsertRequest, UploadedImFile,
};
use im_feishu::FeishuGateway;
use tokio::sync::Mutex;

use super::feishu_topic_channel_id;
use crate::app::FEISHU_CHANNEL;

pub(crate) struct LocalImFileBridge {
    gateways: HashMap<String, FeishuGateway>,
    auto_open_sub_session_channel: bool,
    topic_anchors: Mutex<HashMap<String, String>>,
    topic_anchors_path: Option<std::path::PathBuf>,
}

impl LocalImFileBridge {
    pub(crate) fn new(gateway: Option<FeishuGateway>) -> Self {
        let gateways = gateway
            .map(|gateway| HashMap::from([(FEISHU_CHANNEL.to_string(), gateway)]))
            .unwrap_or_default();
        Self {
            gateways,
            auto_open_sub_session_channel: false,
            topic_anchors: Mutex::new(HashMap::new()),
            topic_anchors_path: None,
        }
    }

    pub(crate) fn with_gateways(
        gateways: HashMap<String, FeishuGateway>,
        auto_open_sub_session_channel: bool,
    ) -> Self {
        Self {
            gateways,
            auto_open_sub_session_channel,
            topic_anchors: Mutex::new(HashMap::new()),
            topic_anchors_path: None,
        }
    }

    pub(crate) fn with_topic_anchors_path(
        mut self,
        path: std::path::PathBuf,
    ) -> anyhow::Result<Self> {
        let anchors = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => return Err(error.into()),
        };
        self.topic_anchors = Mutex::new(anchors);
        self.topic_anchors_path = Some(path);
        Ok(self)
    }

    fn gateway(&self, platform: &str) -> Option<&FeishuGateway> {
        self.gateways.get(platform).or_else(|| {
            (platform == FEISHU_CHANNEL && self.gateways.len() == 1)
                .then(|| self.gateways.values().next())
                .flatten()
        })
    }
}

impl ImFileBridge for LocalImFileBridge {
    fn sub_session_reply_anchor<'a>(
        &'a self,
        platform: &'a str,
        channel_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async move {
            self.topic_anchors
                .lock()
                .await
                .get(&format!("{platform}:{channel_id}"))
                .cloned()
        })
    }
    fn download<'a>(
        &'a self,
        req: ImDownloadRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<DownloadedImFile>> + Send + 'a>,
    > {
        Box::pin(async move {
            let Some(gateway) = self.gateway(&req.platform) else {
                anyhow::bail!("current transport does not support IM file download");
            };
            if req.platform != FEISHU_CHANNEL && !req.platform.starts_with("feishu:") {
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
            let Some(gateway) = self.gateway(&req.platform) else {
                anyhow::bail!("current transport does not support IM file upload");
            };
            if req.platform != FEISHU_CHANNEL && !req.platform.starts_with("feishu:") {
                anyhow::bail!("unsupported platform: {}", req.platform);
            }
            // Both model-authored resources and im_upload use this shared path.
            let file_type = if req.file_type == "image"
                || (req.file_type != "folder" && is_native_image_mime(&req.mime_type))
            {
                "image"
            } else {
                &req.file_type
            };
            let file_key = gateway
                .upload_file(&req.file_name, &req.mime_type, &req.content, file_type)
                .await?;
            let sent_message_id = if !req.message_id.is_empty() {
                match gateway
                    .reply_file(&req.message_id, &file_key, file_type)
                    .await
                {
                    Ok(message_id) => message_id,
                    Err(_) => {
                        gateway
                            .send_file(&req.chat_id, &file_key, file_type)
                            .await?
                    }
                }
            } else {
                gateway
                    .send_file(&req.chat_id, &file_key, file_type)
                    .await?
            };
            Ok(UploadedImFile {
                file_name: req.file_name,
                file_key: file_key.clone(),
                message_id: sent_message_id.clone(),
                resource_url: gateway.file_resource_url(&sent_message_id, &file_key, file_type),
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
            if matches!(request.kind.as_str(), "acp" | "agent")
                && !self.auto_open_sub_session_channel
            {
                return Ok(None);
            }
            if request.platform != FEISHU_CHANNEL && !request.platform.starts_with("feishu:") {
                return Ok(None);
            }
            let Some(gateway) = self.gateway(&request.platform) else {
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
                let (message_id, thread_id) = gateway
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
                let channel_id = feishu_topic_channel_id(&request.parent_channel_id, &thread_id);
                let mut anchors = self.topic_anchors.lock().await;
                anchors.insert(format!("{}:{channel_id}", request.platform), message_id);
                if let Some(path) = &self.topic_anchors_path {
                    if let Some(parent) = path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    crate::atomic_file::write(path, &serde_json::to_vec(&*anchors)?)?;
                }
                return Ok(Some(BoundImChannel {
                    platform: request.platform.clone(),
                    channel_id,
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
                platform: request.platform.clone(),
                channel_id: chat_id,
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_platform_selects_the_matching_gateway() {
        let bridge = LocalImFileBridge::with_gateways(
            HashMap::from([
                (
                    "feishu:work".to_string(),
                    FeishuGateway::new("work-app", "work-secret"),
                ),
                (
                    "feishu:travel".to_string(),
                    FeishuGateway::new("travel-app", "travel-secret"),
                ),
            ]),
            false,
        );
        assert!(bridge.gateway("feishu:work").is_some());
        assert!(bridge.gateway("feishu:travel").is_some());
        assert!(bridge.gateway("feishu:missing").is_none());
    }

    #[tokio::test]
    async fn disabled_sub_session_channels_skip_remote_creation() {
        let bridge =
            LocalImFileBridge::new(Some(FeishuGateway::new("unused-app", "unused-secret")));
        for kind in ["acp", "agent"] {
            let binding = bridge
                .sub_session_binding_upsert(SubSessionBindingUpsertRequest {
                    parent_session_id: "parent".into(),
                    sub_session_id: "child".into(),
                    kind: kind.into(),
                    target: "test".into(),
                    title: None,
                    platform: FEISHU_CHANNEL.into(),
                    parent_channel_id: "chat".into(),
                    parent_thread_id: Some("topic".into()),
                    actor_user_id: None,
                })
                .await
                .unwrap();
            assert!(binding.is_none());
        }
    }
}

fn is_native_image_mime(mime: &str) -> bool {
    matches!(
        mime.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "image/tiff"
    )
}

#[cfg(test)]
mod image_tests {
    #[test]
    fn native_images_are_separate_from_other_file_formats() {
        assert!(super::is_native_image_mime("image/png"));
        assert!(super::is_native_image_mime("Image/JPEG; charset=binary"));
        assert!(!super::is_native_image_mime("image/svg+xml"));
        assert!(!super::is_native_image_mime("application/pdf"));
    }
}
