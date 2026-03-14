use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{
    AgentError, ResumePayload, Tool, ToolContext, ToolOutput, ToolResult,
};
use remi_agentloop::tool::registry::DefaultToolRegistry;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImAttachment {
    pub key: String,
    pub name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub file_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImDocument {
    pub url: String,
    pub title: String,
    pub doc_type: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct ImDownloadRequest {
    pub platform: String,
    pub message_id: String,
    pub chat_id: String,
    pub attachment_key: Option<String>,
    pub document_url: Option<String>,
    pub file_type: String,
}

#[derive(Debug, Clone)]
pub struct DownloadedImFile {
    pub file_name: String,
    pub mime_type: String,
    pub content: Vec<u8>,
    pub source_label: String,
}

#[derive(Debug, Clone)]
pub struct ImUploadRequest {
    pub platform: String,
    pub message_id: String,
    pub chat_id: String,
    pub file_name: String,
    pub mime_type: String,
    pub content: Vec<u8>,
    pub file_type: String,
}

#[derive(Debug, Clone)]
pub struct UploadedImFile {
    pub file_name: String,
    pub file_key: String,
    pub message_id: String,
    pub resource_url: String,
}

pub trait ImFileBridge: Send + Sync {
    fn download<'a>(
        &'a self,
        request: ImDownloadRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<DownloadedImFile>> + Send + 'a>>;

    fn upload<'a>(
        &'a self,
        request: ImUploadRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<UploadedImFile>> + Send + 'a>>;
}

pub fn register_im_tools(
    registry: &mut DefaultToolRegistry,
    root: PathBuf,
    bridge: Arc<dyn ImFileBridge>,
) {
    registry.register(ImDownloadTool {
        root: root.clone(),
        bridge: Arc::clone(&bridge),
    });
    registry.register(ImUploadTool { root, bridge });
}

struct ImDownloadTool {
    root: PathBuf,
    bridge: Arc<dyn ImFileBridge>,
}

impl Tool for ImDownloadTool {
    fn name(&self) -> &str {
        "im_download"
    }

    fn description(&self) -> &str {
        "Download a file attachment, Feishu document link, or Feishu Drive file link from the current IM conversation into the workspace. Defaults to the only available current-message attachment or document when unambiguous. Drive file links (feishu.cn/drive/file/...) are downloaded via the Drive API."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "attachment_key": {
                    "type": "string",
                    "description": "Attachment key from the current IM message. Optional if exactly one attachment is available."
                },
                "document_url": {
                    "type": "string",
                    "description": "Feishu document link from the current IM message. Optional if exactly one document link is available."
                },
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative destination path. Defaults to im/downloads/<suggested_name>."
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Overwrite the destination if it already exists. Defaults to false."
                }
            },
            "additionalProperties": false
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let root = self.root.clone();
        let bridge = Arc::clone(&self.bridge);
        let metadata = ctx.metadata.clone();
        async move {
            Ok(ToolResult::Output(stream! {
                let Some(meta) = metadata.as_ref() else {
                    yield ToolOutput::text("error: im_download requires request metadata");
                    return;
                };

                let platform = meta.get("platform").and_then(|v| v.as_str()).unwrap_or("");
                let message_id = meta.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
                let chat_id = meta.get("thread_id").and_then(|v| v.as_str()).unwrap_or("");
                if platform.is_empty() || message_id.is_empty() || chat_id.is_empty() {
                    yield ToolOutput::text("error: current IM platform context is incomplete");
                    return;
                }

                let attachments = parse_attachments(meta.get("im_attachments").cloned());
                let documents = parse_documents(meta.get("im_documents").cloned());
                let requested_attachment = arguments["attachment_key"].as_str().map(str::to_string);
                let requested_document = arguments["document_url"].as_str().map(str::to_string);

                let (attachment_key, document_url, file_type) = match select_download_source(
                    requested_attachment,
                    requested_document,
                    &attachments,
                    &documents,
                ) {
                    Ok(selection) => selection,
                    Err(err) => {
                        yield ToolOutput::text(format!("error: {err}"));
                        return;
                    }
                };

                let request = ImDownloadRequest {
                    platform: platform.to_string(),
                    message_id: message_id.to_string(),
                    chat_id: chat_id.to_string(),
                    attachment_key,
                    document_url,
                    file_type,
                };

                let downloaded = match bridge.download(request).await {
                    Ok(downloaded) => downloaded,
                    Err(err) => {
                        yield ToolOutput::text(format!("error: {err}"));
                        return;
                    }
                };

                let requested_path = arguments["path"].as_str();
                let overwrite = arguments["overwrite"].as_bool().unwrap_or(false);
                let target = match choose_download_target(&root, requested_path, &downloaded.file_name, overwrite).await {
                    Ok(path) => path,
                    Err(err) => {
                        yield ToolOutput::text(format!("error: {err}"));
                        return;
                    }
                };

                if let Some(parent) = target.parent() {
                    if let Err(err) = tokio::fs::create_dir_all(parent).await {
                        yield ToolOutput::text(format!("error: create parent directory failed: {err}"));
                        return;
                    }
                }
                if let Err(err) = tokio::fs::write(&target, &downloaded.content).await {
                    yield ToolOutput::text(format!("error: write downloaded file failed: {err}"));
                    return;
                }

                let relative = relative_workspace_path(&root, &target);
                let result = serde_json::json!({
                    "workspace_path": relative,
                    "file_name": downloaded.file_name,
                    "mime_type": downloaded.mime_type,
                    "size_bytes": downloaded.content.len(),
                    "source": downloaded.source_label,
                });
                yield ToolOutput::text(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()));
            }))
        }
    }
}

struct ImUploadTool {
    root: PathBuf,
    bridge: Arc<dyn ImFileBridge>,
}

impl Tool for ImUploadTool {
    fn name(&self) -> &str {
        "im_upload"
    }

    fn description(&self) -> &str {
        "Upload a local workspace file to the current IM conversation and return the Feishu file identifiers and link."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the local file to upload."
                },
                "file_name": {
                    "type": "string",
                    "description": "Optional file name to present on IM. Defaults to the source file name."
                },
                "mime_type": {
                    "type": "string",
                    "description": "Optional MIME type override. Defaults to a best-effort guess from the file extension."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let root = self.root.clone();
        let bridge = Arc::clone(&self.bridge);
        let metadata = ctx.metadata.clone();
        async move {
            Ok(ToolResult::Output(stream! {
                let Some(meta) = metadata.as_ref() else {
                    yield ToolOutput::text("error: im_upload requires request metadata");
                    return;
                };

                let platform = meta.get("platform").and_then(|v| v.as_str()).unwrap_or("");
                let message_id = meta.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
                let chat_id = meta.get("thread_id").and_then(|v| v.as_str()).unwrap_or("");
                if platform.is_empty() || chat_id.is_empty() {
                    yield ToolOutput::text("error: current IM platform context is incomplete");
                    return;
                }

                let Some(path_arg) = arguments["path"].as_str() else {
                    yield ToolOutput::text("error: path is required");
                    return;
                };
                let source_path = rooted_path(&root, path_arg);
                let content = match tokio::fs::read(&source_path).await {
                    Ok(content) => content,
                    Err(err) => {
                        yield ToolOutput::text(format!("error: unable to read upload source: {err}"));
                        return;
                    }
                };

                let file_name = arguments["file_name"].as_str().map(str::to_string).unwrap_or_else(|| {
                    source_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("upload.bin")
                        .to_string()
                });
                let mime_type = arguments["mime_type"].as_str().map(str::to_string).unwrap_or_else(|| guess_mime_type(&source_path));

                let uploaded = match bridge.upload(ImUploadRequest {
                    platform: platform.to_string(),
                    message_id: message_id.to_string(),
                    chat_id: chat_id.to_string(),
                    file_name: file_name.clone(),
                    mime_type: mime_type.clone(),
                    content,
                    file_type: String::new(),
                }).await {
                    Ok(uploaded) => uploaded,
                    Err(err) => {
                        yield ToolOutput::text(format!("error: {err}"));
                        return;
                    }
                };

                let result = serde_json::json!({
                    "workspace_path": relative_workspace_path(&root, &source_path),
                    "file_name": uploaded.file_name,
                    "mime_type": mime_type,
                    "file_key": uploaded.file_key,
                    "message_id": uploaded.message_id,
                    "resource_url": uploaded.resource_url,
                });
                yield ToolOutput::text(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()));
            }))
        }
    }
}

fn parse_attachments(value: Option<serde_json::Value>) -> Vec<ImAttachment> {
    let Some(v) = value else { return Vec::new() };
    match v {
        serde_json::Value::String(s) => serde_json::from_str(&s).unwrap_or_default(),
        other => serde_json::from_value(other).unwrap_or_default(),
    }
}

fn parse_documents(value: Option<serde_json::Value>) -> Vec<ImDocument> {
    let Some(v) = value else { return Vec::new() };
    match v {
        serde_json::Value::String(s) => serde_json::from_str(&s).unwrap_or_default(),
        other => serde_json::from_value(other).unwrap_or_default(),
    }
}

fn select_download_source(
    requested_attachment: Option<String>,
    requested_document: Option<String>,
    attachments: &[ImAttachment],
    documents: &[ImDocument],
) -> anyhow::Result<(Option<String>, Option<String>, String)> {
    if requested_attachment.is_some() && requested_document.is_some() {
        anyhow::bail!("attachment_key and document_url are mutually exclusive");
    }
    if let Some(key) = requested_attachment {
        // When the key is provided explicitly, look up the attachment to get file_type
        let file_type = attachments
            .iter()
            .find(|a| a.key == key)
            .map(|a| a.file_type.clone())
            .unwrap_or_default();
        return Ok((Some(key), None, file_type));
    }
    if let Some(url) = requested_document {
        return Ok((None, Some(url), String::new()));
    }

    let candidates = attachments.len() + documents.len();
    if candidates == 0 {
        anyhow::bail!("no downloadable IM attachment or document link is available in the current message");
    }
    if candidates > 1 {
        anyhow::bail!("multiple downloadable items are available; specify attachment_key or document_url explicitly");
    }
    if let Some(attachment) = attachments.first() {
        return Ok((Some(attachment.key.clone()), None, attachment.file_type.clone()));
    }
    if let Some(document) = documents.first() {
        return Ok((None, Some(document.url.clone()), String::new()));
    }

    anyhow::bail!("unable to determine download source")
}

async fn choose_download_target(
    root: &Path,
    requested_path: Option<&str>,
    suggested_name: &str,
    overwrite: bool,
) -> anyhow::Result<PathBuf> {
    let desired = match requested_path {
        Some(path) if !path.trim().is_empty() => rooted_path(root, path),
        _ => rooted_path(root, &format!("im/downloads/{}", sanitize_file_name(suggested_name))),
    };

    if overwrite || tokio::fs::metadata(&desired).await.is_err() {
        return Ok(desired);
    }

    let stem = desired
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let ext = desired.extension().and_then(|value| value.to_str()).unwrap_or("");
    let suffix = Uuid::new_v4().simple().to_string();
    let file_name = if ext.is_empty() {
        format!("{}_{}", stem, suffix)
    } else {
        format!("{}_{}.{}", stem, suffix, ext)
    };
    Ok(desired.with_file_name(file_name))
}

fn rooted_path(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

fn relative_workspace_path(root: &Path, full: &Path) -> String {
    full.strip_prefix(root)
        .unwrap_or(full)
        .to_string_lossy()
        .replace('\\', "/")
}

fn sanitize_file_name(name: &str) -> String {
    let filtered: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    if filtered.is_empty() {
        "file.bin".to_string()
    } else {
        filtered
    }
}

fn guess_mime_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" | "md" => "text/plain".into(),
        "json" => "application/json".into(),
        "csv" => "text/csv".into(),
        "pdf" => "application/pdf".into(),
        "png" => "image/png".into(),
        "jpg" | "jpeg" => "image/jpeg".into(),
        "gif" => "image/gif".into(),
        "zip" => "application/zip".into(),
        _ => "application/octet-stream".into(),
    }
}