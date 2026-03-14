use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    AgentError, ResumePayload, Tool, ToolContext, ToolOutput, ToolResult,
};
use remi_agentloop::tool::registry::DefaultToolRegistry;
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

const FETCH_TIMEOUT_SECS: u64 = 30;
const FETCH_USER_AGENT: &str = "remi-cat-fetch/0.1";
const FETCH_ASYNC_THRESHOLD_MS: u64 = 1_500;
const FETCH_TASK_POLL_INTERVAL_MS: u64 = 100;
const FETCH_DEFAULT_SPEED_BPS: f64 = 512.0 * 1024.0;
const AGENT_FILE_KEY_SEPARATOR: char = '\\';
const LEGACY_AGENT_FILE_KEY_SEPARATOR: char = '\t';

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
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<DownloadedImFile>> + Send + 'a>,
    >;

    fn upload<'a>(
        &'a self,
        request: ImUploadRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<UploadedImFile>> + Send + 'a>,
    >;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedAgentFileKey {
    pub message_id: String,
    pub file_key: String,
}

#[derive(Debug, Clone)]
struct FetchedFile {
    file_name: String,
    mime_type: String,
    content: Vec<u8>,
    source_label: String,
    mode: &'static str,
    resolved_url: Option<String>,
}

#[derive(Debug, Clone)]
enum FetchSource {
    FileKey { file_key: String, file_type: String },
    FeishuDocumentUrl { url: String },
    GenericUrl { url: String },
}

#[derive(Debug, Clone)]
struct FetchCompletion {
    fetched: FetchedFile,
    downloaded_bytes: u64,
}

#[derive(Debug, Clone)]
struct CompletedFetchResult {
    workspace_path: String,
    file_name: String,
    mime_type: String,
    size_bytes: u64,
    source: String,
    mode: String,
    resolved_url: Option<String>,
}

#[derive(Debug, Clone)]
enum FetchTaskStatus {
    Running,
    Completed(CompletedFetchResult),
    Failed(String),
}

#[derive(Debug, Clone)]
struct FetchTaskState {
    started_at: Instant,
    source_label: String,
    total_bytes: Option<u64>,
    downloaded_bytes: u64,
    speed_bps_hint: f64,
    status: FetchTaskStatus,
}

#[derive(Debug, Clone)]
struct FetchTaskRegistry {
    tasks: Arc<Mutex<HashMap<String, FetchTaskState>>>,
    average_speed_bps: Arc<RwLock<f64>>,
}

#[derive(Debug, Clone)]
struct FetchProgressReporter {
    task_id: String,
    tasks: FetchTaskRegistry,
}

impl FetchTaskRegistry {
    fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            average_speed_bps: Arc::new(RwLock::new(FETCH_DEFAULT_SPEED_BPS)),
        }
    }

    async fn create_task(&self, source_label: String, total_bytes: Option<u64>) -> String {
        let task_id = Uuid::new_v4().to_string();
        let speed_bps_hint = self.average_speed_bps().await;
        self.tasks.lock().await.insert(
            task_id.clone(),
            FetchTaskState {
                started_at: Instant::now(),
                source_label,
                total_bytes,
                downloaded_bytes: 0,
                speed_bps_hint,
                status: FetchTaskStatus::Running,
            },
        );
        task_id
    }

    async fn average_speed_bps(&self) -> f64 {
        *self.average_speed_bps.read().await
    }

    async fn set_total_bytes(&self, task_id: &str, total_bytes: Option<u64>) {
        let mut tasks = self.tasks.lock().await;
        if let Some(task) = tasks.get_mut(task_id) {
            if total_bytes.is_some() {
                task.total_bytes = total_bytes;
            }
        }
    }

    async fn add_downloaded_bytes(&self, task_id: &str, delta: u64) {
        let mut tasks = self.tasks.lock().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.downloaded_bytes = task.downloaded_bytes.saturating_add(delta);
        }
    }

    async fn set_downloaded_bytes(&self, task_id: &str, downloaded_bytes: u64) {
        let mut tasks = self.tasks.lock().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.downloaded_bytes = downloaded_bytes;
        }
    }

    async fn complete(&self, task_id: &str, result: CompletedFetchResult, downloaded_bytes: u64) {
        let mut tasks = self.tasks.lock().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.downloaded_bytes = downloaded_bytes;
            task.total_bytes = Some(downloaded_bytes);
            task.status = FetchTaskStatus::Completed(result);
        }
    }

    async fn fail(&self, task_id: &str, error: String) {
        let mut tasks = self.tasks.lock().await;
        if let Some(task) = tasks.get_mut(task_id) {
            task.status = FetchTaskStatus::Failed(error);
        }
    }

    async fn snapshot(&self, task_id: &str) -> Option<FetchTaskState> {
        self.tasks.lock().await.get(task_id).cloned()
    }

    async fn take(&self, task_id: &str) -> Option<FetchTaskState> {
        self.tasks.lock().await.remove(task_id)
    }

    async fn wait_for_terminal(&self, task_id: &str, timeout: Duration) -> Option<FetchTaskState> {
        let started = Instant::now();
        loop {
            let snapshot = self.snapshot(task_id).await?;
            if !matches!(snapshot.status, FetchTaskStatus::Running) {
                return Some(snapshot);
            }

            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return None;
            }

            let remaining = timeout.saturating_sub(elapsed);
            tokio::time::sleep(remaining.min(Duration::from_millis(FETCH_TASK_POLL_INTERVAL_MS)))
                .await;
        }
    }

    async fn record_speed_sample(&self, bytes: u64, elapsed: Duration) {
        if bytes == 0 || elapsed.is_zero() {
            return;
        }

        let sample_speed_bps = bytes as f64 / elapsed.as_secs_f64();
        let mut average_speed_bps = self.average_speed_bps.write().await;
        *average_speed_bps = (*average_speed_bps * 0.7) + (sample_speed_bps * 0.3);
    }
}

impl FetchProgressReporter {
    fn new(task_id: String, tasks: FetchTaskRegistry) -> Self {
        Self { task_id, tasks }
    }

    async fn set_total_bytes(&self, total_bytes: Option<u64>) {
        self.tasks.set_total_bytes(&self.task_id, total_bytes).await;
    }

    async fn add_downloaded_bytes(&self, delta: u64) {
        self.tasks.add_downloaded_bytes(&self.task_id, delta).await;
    }

    async fn set_downloaded_bytes(&self, downloaded_bytes: u64) {
        self.tasks
            .set_downloaded_bytes(&self.task_id, downloaded_bytes)
            .await;
    }

    async fn complete(
        &self,
        result: CompletedFetchResult,
        downloaded_bytes: u64,
        elapsed: Duration,
    ) {
        self.tasks
            .complete(&self.task_id, result, downloaded_bytes)
            .await;
        self.tasks
            .record_speed_sample(downloaded_bytes, elapsed)
            .await;
    }

    async fn fail(&self, error: String) {
        self.tasks.fail(&self.task_id, error).await;
    }
}

pub fn encode_agent_file_key(message_id: &str, file_key: &str) -> String {
    let message_id = message_id.trim();
    let file_key = file_key.trim();

    if let Some(decoded) = decode_agent_file_key(file_key) {
        return format!(
            "{}{}{}",
            decoded.message_id, AGENT_FILE_KEY_SEPARATOR, decoded.file_key
        );
    }
    if message_id.is_empty() || file_key.is_empty() {
        return file_key.to_string();
    }
    format!("{message_id}{AGENT_FILE_KEY_SEPARATOR}{file_key}")
}

pub fn decode_agent_file_key(value: &str) -> Option<DecodedAgentFileKey> {
    let value = value.trim();
    for separator in [AGENT_FILE_KEY_SEPARATOR, LEGACY_AGENT_FILE_KEY_SEPARATOR] {
        if let Some((message_id, file_key)) = value.split_once(separator) {
            let message_id = message_id.trim();
            let file_key = file_key.trim().trim_start_matches(|c| {
                c == AGENT_FILE_KEY_SEPARATOR || c == LEGACY_AGENT_FILE_KEY_SEPARATOR
            });
            if !message_id.is_empty() && !file_key.is_empty() {
                return Some(DecodedAgentFileKey {
                    message_id: message_id.to_string(),
                    file_key: file_key.to_string(),
                });
            }
        }
    }
    None
}

pub fn register_fetch_tool(
    registry: &mut DefaultToolRegistry,
    root: PathBuf,
    bridge: Option<Arc<dyn ImFileBridge>>,
) {
    registry.register(FetchTool {
        root,
        bridge,
        tasks: FetchTaskRegistry::new(),
    });
}

pub fn register_im_tools(
    registry: &mut DefaultToolRegistry,
    root: PathBuf,
    bridge: Arc<dyn ImFileBridge>,
) {
    registry.register(ImUploadTool { root, bridge });
}

struct FetchTool {
    root: PathBuf,
    bridge: Option<Arc<dyn ImFileBridge>>,
    tasks: FetchTaskRegistry,
}

impl Tool for FetchTool {
    fn name(&self) -> &str {
        "fetch"
    }

    fn description(&self) -> &str {
        "Fetch a Feishu file_key, a Feishu document URL, or any generic URL into the workspace. Generic HTML pages are saved as Markdown by default; set raw=true to save the original response body."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Existing fetch task id to poll. When provided, do not pass url or file_key."
                },
                "file_key": {
                    "type": "string",
                    "description": "Self-contained Feishu file key returned by the system, encoded as message_id\\file_key. Optional if exactly one current-message attachment is available."
                },
                "url": {
                    "type": "string",
                    "description": "URL to fetch. Feishu document URLs are detected automatically and downloaded through the Feishu export/download APIs."
                },
                "file_type": {
                    "type": "string",
                    "description": "Optional Feishu file type override when fetching a file_key."
                },
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative destination path. Defaults to fetch/downloads/<suggested_name>."
                },
                "overwrite": {
                    "type": "boolean",
                    "description": "Overwrite the destination if it already exists. Defaults to false."
                },
                "raw": {
                    "type": "boolean",
                    "description": "For generic HTML pages, save the original response body instead of Markdown. Defaults to false."
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
        let bridge = self.bridge.clone();
        let metadata = ctx.metadata.clone();
        let tasks = self.tasks.clone();
        async move {
            Ok(ToolResult::Output(stream! {
                if let Some(task_id) = string_arg(&arguments, "task_id") {
                    if has_fetch_start_arguments(&arguments) {
                        yield ToolOutput::text("error: task_id cannot be combined with url, file_key, path, overwrite, raw, or file_type");
                        return;
                    }

                    let average_speed_bps = tasks.average_speed_bps().await;
                    let Some(snapshot) = tasks.snapshot(&task_id).await else {
                        yield ToolOutput::text(format!("error: unknown fetch task_id: {task_id}"));
                        return;
                    };

                    match snapshot.status {
                        FetchTaskStatus::Running => {
                            yield ToolOutput::text(json_text(fetch_running_value(&task_id, &snapshot, average_speed_bps)));
                        }
                        FetchTaskStatus::Completed(_) | FetchTaskStatus::Failed(_) => {
                            let Some(snapshot) = tasks.take(&task_id).await else {
                                yield ToolOutput::text(format!("error: unknown fetch task_id: {task_id}"));
                                return;
                            };
                            yield ToolOutput::text(json_text(fetch_terminal_value(Some(&task_id), snapshot)));
                        }
                    }
                    return;
                }

                let attachments = metadata
                    .as_ref()
                    .map(|meta| parse_attachments(meta.get("im_attachments").cloned()))
                    .unwrap_or_default();
                let documents = metadata
                    .as_ref()
                    .map(|meta| parse_documents(meta.get("im_documents").cloned()))
                    .unwrap_or_default();
                let requested_file_key = string_arg(&arguments, "file_key");
                let requested_url = string_arg(&arguments, "url");
                let requested_path = string_arg(&arguments, "path");
                let explicit_file_type = string_arg(&arguments, "file_type");
                let raw = arguments["raw"].as_bool().unwrap_or(false);
                let overwrite = arguments["overwrite"].as_bool().unwrap_or(false);

                let source = match select_fetch_source(
                    requested_file_key,
                    requested_url,
                    &attachments,
                    &documents,
                ) {
                    Ok(source) => source,
                    Err(err) => {
                        yield ToolOutput::text(format!("error: {err}"));
                        return;
                    }
                };

                let task_id = tasks
                    .create_task(
                        fetch_source_label(&source),
                        known_total_bytes_for_source(&source, &attachments),
                    )
                    .await;
                let progress = FetchProgressReporter::new(task_id.clone(), tasks.clone());
                let task_root = root.clone();
                let task_bridge = bridge.clone();
                let task_metadata = metadata.clone();
                let task_source = source.clone();
                let task_requested_path = requested_path.clone();
                let task_file_type = explicit_file_type.clone();
                tokio::spawn(async move {
                    run_fetch_task(
                        task_root,
                        task_bridge,
                        task_metadata,
                        task_source,
                        task_requested_path,
                        overwrite,
                        task_file_type,
                        raw,
                        progress,
                    )
                    .await;
                });

                let threshold = Duration::from_millis(FETCH_ASYNC_THRESHOLD_MS);
                if tasks.wait_for_terminal(&task_id, threshold).await.is_some() {
                    let Some(snapshot) = tasks.take(&task_id).await else {
                        yield ToolOutput::text(format!("error: fetch task disappeared: {task_id}"));
                        return;
                    };
                    match snapshot.status {
                        FetchTaskStatus::Completed(_) => {
                            yield ToolOutput::text(json_text(fetch_terminal_value(None, snapshot)));
                        }
                        FetchTaskStatus::Failed(error) => {
                            yield ToolOutput::text(format!("error: {error}"));
                        }
                        FetchTaskStatus::Running => {
                            yield ToolOutput::text(json_text(fetch_running_value(
                                &task_id,
                                &snapshot,
                                tasks.average_speed_bps().await,
                            )));
                        }
                    }
                    return;
                }

                let Some(snapshot) = tasks.snapshot(&task_id).await else {
                    yield ToolOutput::text(format!("error: fetch task disappeared: {task_id}"));
                    return;
                };
                match snapshot.status {
                    FetchTaskStatus::Running => {
                        yield ToolOutput::text(json_text(fetch_running_value(
                            &task_id,
                            &snapshot,
                            tasks.average_speed_bps().await,
                        )));
                    }
                    FetchTaskStatus::Completed(_) | FetchTaskStatus::Failed(_) => {
                        let Some(snapshot) = tasks.take(&task_id).await else {
                            yield ToolOutput::text(format!("error: fetch task disappeared: {task_id}"));
                            return;
                        };
                        yield ToolOutput::text(json_text(fetch_terminal_value(None, snapshot)));
                    }
                }
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
                    "file_key": encode_agent_file_key(&uploaded.message_id, &uploaded.file_key),
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

fn select_fetch_source(
    requested_file_key: Option<String>,
    requested_url: Option<String>,
    attachments: &[ImAttachment],
    documents: &[ImDocument],
) -> anyhow::Result<FetchSource> {
    if requested_file_key.is_some() && requested_url.is_some() {
        anyhow::bail!("file_key and url are mutually exclusive");
    }
    if let Some(file_key) = requested_file_key {
        let file_type = attachments
            .iter()
            .find(|a| file_key_matches(&a.key, &file_key))
            .map(|a| a.file_type.clone())
            .unwrap_or_default();
        return Ok(FetchSource::FileKey {
            file_key,
            file_type,
        });
    }
    if let Some(url) = requested_url {
        return classify_url_source(&url);
    }

    let candidates = attachments.len() + documents.len();
    if candidates == 0 {
        anyhow::bail!(
            "fetch requires url or file_key, or an unambiguous current-message attachment/document"
        );
    }
    if candidates > 1 {
        anyhow::bail!("multiple fetchable items are available; specify file_key or url explicitly");
    }
    if let Some(attachment) = attachments.first() {
        return Ok(FetchSource::FileKey {
            file_key: attachment.key.clone(),
            file_type: attachment.file_type.clone(),
        });
    }
    if let Some(document) = documents.first() {
        return Ok(FetchSource::FeishuDocumentUrl {
            url: document.url.clone(),
        });
    }

    anyhow::bail!("unable to determine fetch source")
}

fn has_fetch_start_arguments(arguments: &serde_json::Value) -> bool {
    ["file_key", "url", "file_type", "path"]
        .into_iter()
        .any(|key| string_arg(arguments, key).is_some())
        || arguments["overwrite"].as_bool().unwrap_or(false)
        || arguments["raw"].as_bool().unwrap_or(false)
}

async fn choose_fetch_target(
    root: &Path,
    requested_path: Option<&str>,
    suggested_name: &str,
    overwrite: bool,
) -> anyhow::Result<PathBuf> {
    let desired = match requested_path {
        Some(path) if !path.trim().is_empty() => rooted_path(root, path),
        _ => rooted_path(
            root,
            &format!("fetch/downloads/{}", sanitize_file_name(suggested_name)),
        ),
    };

    if overwrite || tokio::fs::metadata(&desired).await.is_err() {
        return Ok(desired);
    }

    let stem = desired
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let ext = desired
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let suffix = Uuid::new_v4().simple().to_string();
    let file_name = if ext.is_empty() {
        format!("{}_{}", stem, suffix)
    } else {
        format!("{}_{}.{}", stem, suffix, ext)
    };
    Ok(desired.with_file_name(file_name))
}

fn fetch_source_label(source: &FetchSource) -> String {
    match source {
        FetchSource::FileKey { file_key, .. } => format!("file_key:{file_key}"),
        FetchSource::FeishuDocumentUrl { url } | FetchSource::GenericUrl { url } => url.clone(),
    }
}

fn known_total_bytes_for_source(source: &FetchSource, attachments: &[ImAttachment]) -> Option<u64> {
    match source {
        FetchSource::FileKey { file_key, .. } => attachments
            .iter()
            .find(|attachment| file_key_matches(&attachment.key, file_key))
            .map(|attachment| attachment.size_bytes)
            .filter(|size_bytes| *size_bytes > 0),
        _ => None,
    }
}

fn json_text(value: serde_json::Value) -> String {
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

fn fetch_running_value(
    task_id: &str,
    task: &FetchTaskState,
    average_speed_bps: f64,
) -> serde_json::Value {
    let elapsed_seconds = task.started_at.elapsed().as_secs_f64();
    let speed_bps = if task.downloaded_bytes > 0 && elapsed_seconds > 0.0 {
        task.downloaded_bytes as f64 / elapsed_seconds
    } else {
        task.speed_bps_hint.max(average_speed_bps)
    };
    let (estimated_total_seconds, estimated_remaining_seconds) = match task.total_bytes {
        Some(total_bytes) if speed_bps > 0.0 => {
            let total_seconds = total_bytes as f64 / speed_bps;
            let remaining_seconds = (total_seconds - elapsed_seconds).max(0.0);
            (Some(total_seconds), Some(remaining_seconds))
        }
        _ => (None, None),
    };

    serde_json::json!({
        "status": "running",
        "task_id": task_id,
        "source": task.source_label,
        "elapsed_seconds": elapsed_seconds,
        "downloaded_bytes": task.downloaded_bytes,
        "total_bytes": task.total_bytes,
        "estimated_speed_bytes_per_sec": speed_bps,
        "estimated_total_seconds": estimated_total_seconds,
        "estimated_remaining_seconds": estimated_remaining_seconds,
        "poll_hint": format!("Call fetch again with task_id={task_id} to poll the result."),
    })
}

fn fetch_terminal_value(task_id: Option<&str>, task: FetchTaskState) -> serde_json::Value {
    match task.status {
        FetchTaskStatus::Completed(result) => {
            let mut value = serde_json::json!({
                "status": "completed",
                "workspace_path": result.workspace_path,
                "file_name": result.file_name,
                "mime_type": result.mime_type,
                "size_bytes": result.size_bytes,
                "source": result.source,
                "mode": result.mode,
                "elapsed_seconds": task.started_at.elapsed().as_secs_f64(),
            });
            if let Some(task_id) = task_id {
                value["task_id"] = serde_json::Value::String(task_id.to_string());
            }
            if let Some(resolved_url) = result.resolved_url {
                value["resolved_url"] = serde_json::Value::String(resolved_url);
            }
            value
        }
        FetchTaskStatus::Failed(error) => serde_json::json!({
            "status": "failed",
            "task_id": task_id,
            "source": task.source_label,
            "elapsed_seconds": task.started_at.elapsed().as_secs_f64(),
            "error": error,
        }),
        FetchTaskStatus::Running => serde_json::json!({
            "status": "running",
            "task_id": task_id,
            "source": task.source_label,
        }),
    }
}

async fn run_fetch_task(
    root: PathBuf,
    bridge: Option<Arc<dyn ImFileBridge>>,
    metadata: Option<serde_json::Value>,
    source: FetchSource,
    requested_path: Option<String>,
    overwrite: bool,
    explicit_file_type: Option<String>,
    raw: bool,
    progress: FetchProgressReporter,
) {
    let started_at = Instant::now();
    let result = async {
        let completion = execute_fetch_source_with_progress(
            source,
            bridge.as_ref(),
            metadata.as_ref(),
            explicit_file_type,
            raw,
            Some(&progress),
        )
        .await?;
        let target = choose_fetch_target(
            &root,
            requested_path.as_deref(),
            &completion.fetched.file_name,
            overwrite,
        )
        .await?;

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("create parent directory")?;
        }
        tokio::fs::write(&target, &completion.fetched.content)
            .await
            .context("write fetched file")?;

        Ok::<_, anyhow::Error>((
            CompletedFetchResult {
                workspace_path: relative_workspace_path(&root, &target),
                file_name: completion.fetched.file_name,
                mime_type: completion.fetched.mime_type,
                size_bytes: completion.fetched.content.len() as u64,
                source: completion.fetched.source_label,
                mode: completion.fetched.mode.to_string(),
                resolved_url: completion.fetched.resolved_url,
            },
            completion.downloaded_bytes,
        ))
    }
    .await;

    match result {
        Ok((completed, downloaded_bytes)) => {
            progress
                .complete(completed, downloaded_bytes, started_at.elapsed())
                .await;
        }
        Err(err) => {
            progress.fail(err.to_string()).await;
        }
    }
}

async fn execute_fetch_source_with_progress(
    source: FetchSource,
    bridge: Option<&Arc<dyn ImFileBridge>>,
    metadata: Option<&serde_json::Value>,
    explicit_file_type: Option<String>,
    raw: bool,
    progress: Option<&FetchProgressReporter>,
) -> anyhow::Result<FetchCompletion> {
    match source {
        FetchSource::FileKey {
            file_key,
            file_type,
        } => {
            let bridge = bridge.context("fetching a Feishu file_key requires IM bridge support")?;
            let fallback_message_id = metadata_string(metadata, "message_id").unwrap_or_default();
            let chat_id = metadata_string(metadata, "thread_id").unwrap_or_default();
            let decoded = decode_agent_file_key(&file_key);
            let message_id = decoded
                .as_ref()
                .map(|value| value.message_id.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or(fallback_message_id);

            if message_id.is_empty() {
                anyhow::bail!(
                    "file_key must include an embedded message_id or be used in current IM context"
                );
            }

            let resolved_file_type = explicit_file_type
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| {
                    if file_type.trim().is_empty() {
                        "file".to_string()
                    } else {
                        file_type
                    }
                });
            let downloaded = bridge
                .download(ImDownloadRequest {
                    platform: "feishu".to_string(),
                    message_id,
                    chat_id,
                    attachment_key: Some(file_key),
                    document_url: None,
                    file_type: resolved_file_type,
                })
                .await?;
            let downloaded_bytes = downloaded.content.len() as u64;
            if let Some(progress) = progress {
                progress.set_total_bytes(Some(downloaded_bytes)).await;
                progress.set_downloaded_bytes(downloaded_bytes).await;
            }

            Ok(FetchCompletion {
                downloaded_bytes,
                fetched: FetchedFile {
                    file_name: downloaded.file_name,
                    mime_type: downloaded.mime_type,
                    content: downloaded.content,
                    source_label: downloaded.source_label,
                    mode: "feishu_file",
                    resolved_url: None,
                },
            })
        }
        FetchSource::FeishuDocumentUrl { url } => {
            let bridge =
                bridge.context("fetching a Feishu document URL requires IM bridge support")?;
            let message_id = metadata_string(metadata, "message_id").unwrap_or_default();
            let chat_id = metadata_string(metadata, "thread_id").unwrap_or_default();
            let downloaded = bridge
                .download(ImDownloadRequest {
                    platform: "feishu".to_string(),
                    message_id,
                    chat_id,
                    attachment_key: None,
                    document_url: Some(url.clone()),
                    file_type: String::new(),
                })
                .await?;
            let downloaded_bytes = downloaded.content.len() as u64;
            if let Some(progress) = progress {
                progress.set_total_bytes(Some(downloaded_bytes)).await;
                progress.set_downloaded_bytes(downloaded_bytes).await;
            }

            Ok(FetchCompletion {
                downloaded_bytes,
                fetched: FetchedFile {
                    file_name: downloaded.file_name,
                    mime_type: downloaded.mime_type,
                    content: downloaded.content,
                    source_label: downloaded.source_label,
                    mode: "feishu_document",
                    resolved_url: Some(url),
                },
            })
        }
        FetchSource::GenericUrl { url } => {
            fetch_generic_url_with_progress(&url, raw, progress).await
        }
    }
}

#[cfg(test)]
async fn fetch_generic_url(url: &str, raw: bool) -> anyhow::Result<FetchedFile> {
    Ok(fetch_generic_url_with_progress(url, raw, None)
        .await?
        .fetched)
}

async fn fetch_generic_url_with_progress(
    url: &str,
    raw: bool,
    progress: Option<&FetchProgressReporter>,
) -> anyhow::Result<FetchCompletion> {
    let parsed_url = reqwest::Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(FETCH_USER_AGENT)
        .build()
        .context("build fetch HTTP client")?;

    let response = client
        .get(parsed_url)
        .send()
        .await
        .with_context(|| format!("request url: {url}"))?;
    if !response.status().is_success() {
        anyhow::bail!("fetch HTTP {} for {url}", response.status());
    }

    let resolved_url = response.url().to_string();
    let headers = response.headers().clone();
    let total_bytes = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| response.content_length());
    if let Some(progress) = progress {
        progress.set_total_bytes(total_bytes).await;
    }
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let disposition_name = headers
        .get(CONTENT_DISPOSITION)
        .and_then(|value| value.to_str().ok())
        .and_then(content_disposition_filename);
    let mut bytes = Vec::with_capacity(total_bytes.unwrap_or_default() as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read fetch response body")?;
        if let Some(progress) = progress {
            progress.add_downloaded_bytes(chunk.len() as u64).await;
        }
        bytes.extend_from_slice(&chunk);
    }
    let downloaded_bytes = bytes.len() as u64;

    if is_html_content_type(&content_type) && !raw {
        let html = String::from_utf8_lossy(&bytes).into_owned();
        let markdown = html2md::parse_html(&html);
        return Ok(FetchCompletion {
            downloaded_bytes,
            fetched: FetchedFile {
                file_name: markdown_file_name(disposition_name.as_deref(), &resolved_url, &html),
                mime_type: "text/markdown".to_string(),
                content: markdown.into_bytes(),
                source_label: url.to_string(),
                mode: "url_markdown",
                resolved_url: Some(resolved_url),
            },
        });
    }

    let raw_mode = if raw { "url_raw" } else { "url_file" };
    Ok(FetchCompletion {
        downloaded_bytes,
        fetched: FetchedFile {
            file_name: raw_file_name(disposition_name.as_deref(), &resolved_url, &content_type),
            mime_type: content_type,
            content: bytes,
            source_label: url.to_string(),
            mode: raw_mode,
            resolved_url: Some(resolved_url),
        },
    })
}

fn classify_url_source(url: &str) -> anyhow::Result<FetchSource> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
    if is_feishu_document_url(&parsed) {
        Ok(FetchSource::FeishuDocumentUrl {
            url: url.to_string(),
        })
    } else {
        Ok(FetchSource::GenericUrl {
            url: url.to_string(),
        })
    }
}

fn string_arg(arguments: &serde_json::Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn metadata_string(metadata: Option<&serde_json::Value>, key: &str) -> Option<String> {
    metadata
        .and_then(|value| value.get(key))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn is_feishu_document_url(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    if !host.contains("feishu.cn") && !host.contains("larksuite.com") {
        return false;
    }

    let Some(segments) = url.path_segments() else {
        return false;
    };
    let segments: Vec<&str> = segments.filter(|segment| !segment.is_empty()).collect();
    for (index, segment) in segments.iter().enumerate() {
        match *segment {
            "docx" | "docs" | "doc" | "wiki" | "sheet" | "sheets" | "base" | "bitable" => {
                return segments.get(index + 1).is_some();
            }
            "drive" => {
                if segments.get(index + 1).copied() == Some("file") {
                    return segments.get(index + 2).is_some();
                }
            }
            _ => {}
        }
    }
    false
}

fn file_key_matches(candidate: &str, requested: &str) -> bool {
    if candidate == requested {
        return true;
    }

    match (
        decode_agent_file_key(candidate),
        decode_agent_file_key(requested),
    ) {
        (Some(candidate_key), Some(requested_key)) => {
            candidate_key.message_id == requested_key.message_id
                && candidate_key.file_key == requested_key.file_key
        }
        (Some(candidate_key), None) => candidate_key.file_key == requested,
        (None, Some(requested_key)) => candidate == requested_key.file_key,
        (None, None) => false,
    }
}

fn markdown_file_name(disposition_name: Option<&str>, resolved_url: &str, html: &str) -> String {
    if let Some(name) = disposition_name {
        return with_extension(name, "md");
    }
    if let Some(title) = extract_html_title(html) {
        return with_extension(&title, "md");
    }
    if let Ok(url) = reqwest::Url::parse(resolved_url) {
        if let Some(name) = file_name_from_url(&url) {
            return with_extension(&name, "md");
        }
        if let Some(host) = url.host_str() {
            return with_extension(host, "md");
        }
    }
    "page.md".to_string()
}

fn raw_file_name(disposition_name: Option<&str>, resolved_url: &str, content_type: &str) -> String {
    if let Some(name) = disposition_name {
        return sanitize_file_name(name);
    }
    if let Ok(url) = reqwest::Url::parse(resolved_url) {
        if let Some(name) = file_name_from_url(&url) {
            let sanitized = sanitize_file_name(&name);
            if sanitized.contains('.') {
                return sanitized;
            }
            let extension = extension_for_content_type(content_type);
            if !extension.is_empty() {
                return format!("{sanitized}.{extension}");
            }
            return sanitized;
        }
        if let Some(host) = url.host_str() {
            let extension = extension_for_content_type(content_type);
            if extension.is_empty() {
                return sanitize_file_name(host);
            }
            return format!("{}.{}", sanitize_file_name(host), extension);
        }
    }

    let extension = extension_for_content_type(content_type);
    if extension.is_empty() {
        "download".to_string()
    } else {
        format!("download.{extension}")
    }
}

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let title_open = lower[start..].find('>')? + start + 1;
    let title_close = lower[title_open..].find("</title>")? + title_open;
    let title = html[title_open..title_close].trim();
    if title.is_empty() {
        None
    } else {
        Some(sanitize_file_name(title))
    }
}

fn file_name_from_url(url: &reqwest::Url) -> Option<String> {
    url.path_segments()?
        .filter(|segment| !segment.is_empty())
        .last()
        .map(sanitize_file_name)
}

fn with_extension(name: &str, extension: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or(name);
    format!("{}.{}", sanitize_file_name(stem), extension)
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

fn is_html_content_type(content_type: &str) -> bool {
    matches!(content_type, "text/html" | "application/xhtml+xml")
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
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

fn extension_for_content_type(content_type: &str) -> &'static str {
    match content_type {
        "text/plain" => "txt",
        "text/html" | "application/xhtml+xml" => "html",
        "text/markdown" => "md",
        "application/json" => "json",
        "application/pdf" => "pdf",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_url_source, decode_agent_file_key, encode_agent_file_key, fetch_generic_url,
        file_key_matches, markdown_file_name, select_fetch_source, FetchSource, FetchTaskRegistry,
        FetchTool, ImAttachment, ImDocument,
    };
    use futures::StreamExt;
    use std::sync::{Arc, RwLock};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn encodes_agent_file_key_with_backslash_separator() {
        assert_eq!(
            encode_agent_file_key("msg_123", "file_abc"),
            "msg_123\\file_abc"
        );
    }

    #[test]
    fn normalizes_legacy_tab_encoded_file_keys() {
        assert_eq!(
            encode_agent_file_key("ignored", "msg_123\tfile_abc"),
            "msg_123\\file_abc"
        );
    }

    #[test]
    fn decodes_new_and_legacy_agent_file_keys() {
        let new_key = decode_agent_file_key("msg_123\\file_abc").expect("new key should decode");
        assert_eq!(new_key.message_id, "msg_123");
        assert_eq!(new_key.file_key, "file_abc");

        let legacy_key =
            decode_agent_file_key("msg_456\tfile_def").expect("legacy key should decode");
        assert_eq!(legacy_key.message_id, "msg_456");
        assert_eq!(legacy_key.file_key, "file_def");
    }

    #[test]
    fn decodes_double_escaped_backslash_file_keys() {
        let escaped_key =
            decode_agent_file_key("msg_123\\\\file_abc").expect("double escaped key should decode");
        assert_eq!(escaped_key.message_id, "msg_123");
        assert_eq!(escaped_key.file_key, "file_abc");
    }

    #[test]
    fn matches_bare_file_key_against_encoded_attachment() {
        assert!(file_key_matches("msg_123\\file_abc", "file_abc"));
        assert!(file_key_matches("msg_123\tfile_abc", "msg_123\\file_abc"));
        assert!(!file_key_matches("msg_123\\file_abc", "msg_999\\file_abc"));
    }

    #[test]
    fn classifies_feishu_document_urls() {
        let source = classify_url_source("https://foo.feishu.cn/docx/AbCdEf123?from=im")
            .expect("classification should succeed");
        match source {
            FetchSource::FeishuDocumentUrl { .. } => {}
            other => panic!("expected Feishu document source, got {other:?}"),
        }
    }

    #[test]
    fn classifies_generic_urls() {
        let source = classify_url_source("https://example.com/index.html")
            .expect("classification should succeed");
        match source {
            FetchSource::GenericUrl { .. } => {}
            other => panic!("expected generic URL source, got {other:?}"),
        }
    }

    #[test]
    fn fetch_schema_does_not_expose_legacy_download_aliases() {
        let tool = FetchTool {
            root: std::path::PathBuf::new(),
            bridge: None,
            tasks: FetchTaskRegistry::new(),
        };
        let schema = <FetchTool as remi_agentloop::prelude::Tool>::parameters_schema(&tool);
        let properties = schema["properties"]
            .as_object()
            .expect("fetch schema properties should be an object");

        assert!(properties.contains_key("task_id"));
        assert!(properties.contains_key("file_key"));
        assert!(properties.contains_key("url"));
        assert!(!properties.contains_key("attachment_key"));
        assert!(!properties.contains_key("document_url"));
    }

    #[test]
    fn selects_unambiguous_current_message_attachment() {
        let attachments = vec![ImAttachment {
            key: "msg_123\\file_abc".into(),
            file_type: "file".into(),
            ..ImAttachment::default()
        }];
        let source =
            select_fetch_source(None, None, &attachments, &[]).expect("selection should succeed");
        match source {
            FetchSource::FileKey {
                file_key,
                file_type,
            } => {
                assert_eq!(file_key, "msg_123\\file_abc");
                assert_eq!(file_type, "file");
            }
            other => panic!("expected file_key source, got {other:?}"),
        }
    }

    #[test]
    fn selects_unambiguous_current_message_document() {
        let documents = vec![ImDocument {
            url: "https://foo.feishu.cn/wiki/AbCdEf123".into(),
            ..ImDocument::default()
        }];
        let source =
            select_fetch_source(None, None, &[], &documents).expect("selection should succeed");
        match source {
            FetchSource::FeishuDocumentUrl { url } => {
                assert_eq!(url, "https://foo.feishu.cn/wiki/AbCdEf123");
            }
            other => panic!("expected Feishu document source, got {other:?}"),
        }
    }

    #[test]
    fn markdown_file_name_prefers_title_when_needed() {
        let name = markdown_file_name(
            None,
            "https://example.com/",
            "<html><head><title>Quarterly Notes</title></head><body></body></html>",
        );
        assert_eq!(name, "Quarterly_Notes.md");
    }

    #[tokio::test]
    async fn fetches_html_as_markdown_by_default() {
        let body = "<html><head><title>Meeting Notes</title></head><body><h1>Hello</h1><p>World</p></body></html>";
        let url = serve_once(body, "text/html; charset=utf-8", "/notes").await;
        let fetched = fetch_generic_url(&url, false)
            .await
            .expect("fetch should succeed");

        assert_eq!(fetched.mode, "url_markdown");
        assert_eq!(fetched.file_name, "Meeting_Notes.md");
        assert_eq!(fetched.mime_type, "text/markdown");
        let markdown = String::from_utf8(fetched.content).expect("markdown content should decode");
        assert!(markdown.contains("Hello"));
        assert!(markdown.contains("World"));
    }

    #[tokio::test]
    async fn fetches_raw_html_when_requested() {
        let body =
            "<html><head><title>Meeting Notes</title></head><body><p>World</p></body></html>";
        let url = serve_once(body, "text/html", "/notes").await;
        let fetched = fetch_generic_url(&url, true)
            .await
            .expect("fetch should succeed");

        assert_eq!(fetched.mode, "url_raw");
        assert_eq!(fetched.file_name, "notes.html");
        assert_eq!(
            String::from_utf8(fetched.content).expect("raw HTML should decode"),
            body
        );
    }

    #[tokio::test]
    async fn slow_fetch_returns_task_id_and_poll_completes() {
        let root = std::env::temp_dir().join(format!("remi-fetch-test-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        let tool = FetchTool {
            root: root.clone(),
            bridge: None,
            tasks: FetchTaskRegistry::new(),
        };
        let body = vec![b'a'; 256 * 1024];
        let url = serve_slow_chunks(body, "text/plain", "/slow.txt", 8, 300).await;
        let ctx = test_tool_context();

        let started = collect_tool_output(
            <FetchTool as remi_agentloop::prelude::Tool>::execute(
                &tool,
                serde_json::json!({ "url": url }),
                None,
                &ctx,
            )
            .await
            .expect("fetch start should succeed"),
        )
        .await;
        let started: serde_json::Value =
            serde_json::from_str(&started).expect("start output should be valid json");

        assert_eq!(started["status"], "running");
        assert!(started["task_id"].is_string());
        assert!(started["estimated_speed_bytes_per_sec"].is_number());

        let task_id = started["task_id"]
            .as_str()
            .expect("task_id should be present")
            .to_string();

        let mut completed = None;
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let polled = collect_tool_output(
                <FetchTool as remi_agentloop::prelude::Tool>::execute(
                    &tool,
                    serde_json::json!({ "task_id": task_id.clone() }),
                    None,
                    &ctx,
                )
                .await
                .expect("fetch poll should succeed"),
            )
            .await;
            let polled: serde_json::Value =
                serde_json::from_str(&polled).expect("poll output should be valid json");
            match polled["status"].as_str() {
                Some("running") => continue,
                Some("completed") => {
                    completed = Some(polled);
                    break;
                }
                other => panic!("unexpected poll status: {other:?}"),
            }
        }

        let completed = completed.expect("slow fetch should eventually complete");
        let workspace_path = completed["workspace_path"]
            .as_str()
            .expect("workspace_path should be present");
        tokio::fs::metadata(root.join(workspace_path))
            .await
            .expect("completed fetch output should exist on disk");
    }

    async fn serve_once(body: &str, content_type: &str, path: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener bind should succeed");
        let addr = listener
            .local_addr()
            .expect("listener local_addr should succeed");
        let body = body.to_string();
        let content_type = content_type.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept should succeed");
            let mut buffer = [0_u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response write should succeed");
        });
        format!("http://{addr}{path}")
    }

    async fn serve_slow_chunks(
        body: Vec<u8>,
        content_type: &str,
        path: &str,
        chunk_count: usize,
        delay_ms: u64,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener bind should succeed");
        let addr = listener
            .local_addr()
            .expect("listener local_addr should succeed");
        let content_type = content_type.to_string();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept should succeed");
            let mut buffer = [0_u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("headers write should succeed");

            let chunk_size = (body.len() / chunk_count).max(1);
            for chunk in body.chunks(chunk_size) {
                socket
                    .write_all(chunk)
                    .await
                    .expect("chunk write should succeed");
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
        });
        format!("http://{addr}{path}")
    }

    fn test_tool_context() -> remi_agentloop::prelude::ToolContext {
        remi_agentloop::prelude::ToolContext {
            config: remi_agentloop::prelude::AgentConfig::default(),
            thread_id: Some(
                serde_json::from_value(serde_json::json!("test-thread"))
                    .expect("thread_id should deserialize"),
            ),
            run_id: serde_json::from_value(serde_json::json!("test-run"))
                .expect("run_id should deserialize"),
            metadata: None,
            user_state: Arc::new(RwLock::new(serde_json::Value::Null)),
        }
    }

    async fn collect_tool_output(
        result: remi_agentloop::prelude::ToolResult<
            impl futures::Stream<Item = remi_agentloop::prelude::ToolOutput>,
        >,
    ) -> String {
        match result {
            remi_agentloop::prelude::ToolResult::Interrupt(_) => "interrupted".to_string(),
            remi_agentloop::prelude::ToolResult::Output(output) => {
                let mut output = std::pin::pin!(output);
                let mut last = String::new();
                while let Some(item) = output.next().await {
                    if let remi_agentloop::prelude::ToolOutput::Result(content) = item {
                        last = content.text_content();
                    }
                }
                last
            }
        }
    }
}
