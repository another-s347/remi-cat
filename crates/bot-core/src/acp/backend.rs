use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{
    ContentBlock as AcpContentBlock, ContentChunk as AcpContentChunk, InitializeRequest,
    LoadSessionRequest, NewSessionRequest, NewSessionResponse, SessionNotification,
    SessionUpdate as AcpSessionUpdate,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::util::MatchDispatch;
use agent_client_protocol::{AcpAgent, Agent, Client, SessionMessage};
use anyhow::{Context, Result};
use chrono::Utc;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::approval::{ToolApprovalDecision, ToolApprovalRequest};
use crate::im_tools::{AcpBindingDeleteRequest, AcpBindingUpsertRequest, ImFileBridge};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpBoundChannel {
    pub platform: String,
    pub channel_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AcpTurnRecord {
    user_message: String,
    assistant_reply: String,
    summary: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AcpSessionRecord {
    session_id: String,
    sub_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    created_at: String,
    updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    external_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bound_channel: Option<AcpBoundChannel>,
    #[serde(default)]
    transcript: Vec<AcpTurnRecord>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AcpStore {
    #[serde(default)]
    sessions: HashMap<String, AcpSessionRecord>,
}

#[derive(Debug, Clone)]
enum AcpConfig {
    Local {
        client: AcpRemoteClient,
        agent_name: String,
        local_args: Vec<String>,
        local_bin: Option<String>,
    },
    Remote {
        base_url: String,
        client: AcpRemoteClient,
        agent_name: String,
        api_key: Option<String>,
        model: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AcpRemoteClient {
    Remi,
    Other(String),
}

impl AcpRemoteClient {
    fn from_env() -> Self {
        match std::env::var("REMI_ACP_CLIENT")
            .ok()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "remi" | "" => Self::Remi,
            other => Self::Other(other.to_string()),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Remi => "remi",
            Self::Other(value) => value,
        }
    }

    fn is_codex(&self) -> bool {
        matches!(self, Self::Other(value) if value == "codex")
    }
}

impl std::fmt::Display for AcpRemoteClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AcpConfig {
    fn from_env() -> Self {
        let agent_name = std::env::var("REMI_ACP_AGENT_NAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "default".to_string());
        let mode = std::env::var("REMI_ACP_MODE")
            .ok()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let client = AcpRemoteClient::from_env();
        if mode == "local" || mode == "stub" {
            let local_args = local_acp_args_from_env(&client);
            let local_bin = local_acp_binary_from_env(&client);
            return Self::Local {
                client,
                agent_name,
                local_args,
                local_bin,
            };
        }

        if let Ok(base_url) = std::env::var("REMI_ACP_BASE_URL") {
            let base_url = base_url.trim().trim_end_matches('/').to_string();
            if !base_url.is_empty() {
                let api_key = std::env::var("REMI_ACP_API_KEY")
                    .ok()
                    .filter(|value| !value.trim().is_empty());
                let model = std::env::var("REMI_ACP_MODEL")
                    .ok()
                    .filter(|value| !value.trim().is_empty());
                return Self::Remote {
                    base_url,
                    client,
                    agent_name,
                    api_key,
                    model,
                };
            }
        }

        let local_args = local_acp_args_from_env(&client);
        let local_bin = local_acp_binary_from_env(&client);
        Self::Local {
            client,
            agent_name,
            local_args,
            local_bin,
        }
    }

    fn agent_name(&self) -> &str {
        match self {
            Self::Local { agent_name, .. } | Self::Remote { agent_name, .. } => agent_name.as_str(),
        }
    }

    fn client(&self) -> &AcpRemoteClient {
        match self {
            Self::Local { client, .. } | Self::Remote { client, .. } => client,
        }
    }

    fn tool_name(&self) -> String {
        std::env::var("REMI_ACP_TOOL_NAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| default_tool_name_for_client(self.client().as_str()))
    }
}

fn default_tool_name_for_client(client: &str) -> String {
    if client.trim().eq_ignore_ascii_case("codex") {
        return "codex".to_string();
    }
    format!("acp__{}", client.trim().replace('-', "_"))
}

#[derive(Debug, Clone)]
pub struct AcpToolRequest {
    pub message: String,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub current_channel: Option<AcpBoundChannel>,
    pub startup_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolResponse {
    pub session_id: String,
    pub reply: String,
    pub final_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolTaskStatus {
    pub status: String,
    pub task_id: String,
    pub session_id: String,
    pub sub_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<AcpToolResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AcpToolTaskEvent {
    Delta(String),
    Thinking(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolDelta {
        id: String,
        name: String,
        delta: String,
    },
    ToolResult {
        id: String,
        name: String,
        result: String,
    },
    ApprovalRequested(ToolApprovalRequest),
    ApprovalUpdated(ToolApprovalRequest),
    ApprovalResolved {
        request: ToolApprovalRequest,
        decision: ToolApprovalDecision,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpApprovalDecision {
    pub approval_id: String,
    pub decision: ToolApprovalDecision,
}

pub struct AcpSpawnedToolTask {
    pub task_id: String,
    pub session_id: String,
    pub sub_session_id: String,
    pub events: mpsc::UnboundedReceiver<AcpToolTaskEvent>,
    pub decisions: Option<mpsc::UnboundedSender<AcpApprovalDecision>>,
}

#[derive(Debug, Clone)]
pub struct PreparedToolTurn {
    pub session_id: String,
    pub sub_session_id: String,
    pub external_session_id: Option<String>,
    pub title: Option<String>,
    pub prompt: String,
    pub message: String,
    pub startup_args: Vec<String>,
}

#[derive(Debug, Clone)]
struct AcpInvocationResult {
    reply: String,
    external_session_id: Option<String>,
}

impl AcpInvocationResult {
    fn new(reply: String) -> Self {
        Self {
            reply,
            external_session_id: None,
        }
    }
}

pub trait AcpLocalRunner: Send + Sync {
    fn run<'a>(
        &'a self,
        session_id: &'a str,
        message: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + 'a>>;
}

pub struct AcpBackend {
    config: AcpConfig,
    store_path: PathBuf,
    store: Mutex<AcpStore>,
    tool_tasks: Mutex<HashMap<String, AcpToolTask>>,
    im_bridge: Option<Arc<dyn ImFileBridge>>,
    local_runner: std::sync::RwLock<Option<Arc<dyn AcpLocalRunner>>>,
}

struct AcpToolTask {
    session_id: String,
    sub_session_id: String,
    handle: JoinHandle<Result<AcpToolResponse>>,
}

impl AcpBackend {
    pub fn new(data_dir: PathBuf, im_bridge: Option<Arc<dyn ImFileBridge>>) -> Self {
        let store_path = data_dir.join("acp").join("sessions.json");
        let store = load_store(&store_path).unwrap_or_default();
        Self {
            config: AcpConfig::from_env(),
            store_path,
            store: Mutex::new(store),
            tool_tasks: Mutex::new(HashMap::new()),
            im_bridge,
            local_runner: std::sync::RwLock::new(None),
        }
    }

    pub fn set_local_runner(&self, runner: Arc<dyn AcpLocalRunner>) {
        if let Ok(mut slot) = self.local_runner.write() {
            *slot = Some(runner);
        }
    }

    #[cfg(test)]
    pub(crate) fn has_local_runner(&self) -> bool {
        self.local_runner
            .read()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    pub fn is_enabled(&self) -> bool {
        true
    }

    pub fn client_name(&self) -> &str {
        self.config.client().as_str()
    }

    pub fn active_tool_available(&self) -> bool {
        match &self.config {
            AcpConfig::Local {
                client, local_bin, ..
            } => match client {
                AcpRemoteClient::Remi => match local_bin {
                    Some(bin) => local_acp_binary_available(bin),
                    None => true,
                },
                AcpRemoteClient::Other(_) => local_bin
                    .as_deref()
                    .map(local_acp_binary_available)
                    .unwrap_or(false),
            },
            AcpConfig::Remote { base_url, .. } => !base_url.trim().is_empty(),
        }
    }

    pub fn active_tool_name(&self) -> String {
        self.config.tool_name()
    }

    pub fn active_tool_description(&self) -> String {
        format!(
            "Open or resume an ACP session using the `{}` client.",
            self.config.client()
        )
    }

    pub async fn prepare_tool_turn(&self, request: AcpToolRequest) -> Result<PreparedToolTurn> {
        let started = Instant::now();
        let message = request.message.trim().to_string();
        if message.is_empty() {
            anyhow::bail!("missing ACP message");
        }
        if !request.startup_args.is_empty() {
            anyhow::bail!(
                "startup_args are not supported by the ACP backend; configure local adapter args in the ACP profile"
            );
        }
        tracing::info!(
            acp_session_id = request.session_id.as_deref().unwrap_or(""),
            title_present = request
                .title
                .as_ref()
                .is_some_and(|value| !value.trim().is_empty()),
            message_len = message.len(),
            platform = request
                .current_channel
                .as_ref()
                .map(|channel| channel.platform.as_str())
                .unwrap_or(""),
            channel_id = request
                .current_channel
                .as_ref()
                .map(|channel| channel.channel_id.as_str())
                .unwrap_or(""),
            "acp.prepare.start"
        );

        let mut store = self.store.lock().await;
        let now = Utc::now().to_rfc3339();
        let is_new = request
            .session_id
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty();
        let session_id = request
            .session_id
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        if is_new {
            let mut record = AcpSessionRecord {
                session_id: session_id.clone(),
                sub_session_id: Uuid::new_v4().to_string(),
                title: request.title.clone(),
                created_at: now.clone(),
                updated_at: now.clone(),
                external_session_id: None,
                last_summary: None,
                bound_channel: None,
                transcript: Vec::new(),
            };
            if let Some(channel) = request.current_channel.clone() {
                self.ensure_channel_binding(&session_id, &channel).await?;
                record.bound_channel = Some(channel);
            }
            store.sessions.insert(session_id.clone(), record);
            save_store(&self.store_path, &store)?;
        }

        let record = store
            .sessions
            .get(&session_id)
            .cloned()
            .with_context(|| format!("ACP session not found: {session_id}"))?;

        let prompt = build_prompt_with_recent_summaries(
            &self.config,
            &store,
            &session_id,
            &record.transcript,
            &message,
        );

        tracing::info!(
            acp_session_id = %session_id,
            sub_session_id = %record.sub_session_id,
            is_new,
            transcript_turns = record.transcript.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "acp.prepare.completed"
        );

        Ok(PreparedToolTurn {
            session_id,
            sub_session_id: record.sub_session_id,
            external_session_id: record.external_session_id,
            title: record.title,
            prompt,
            message,
            startup_args: request.startup_args,
        })
    }

    async fn ensure_channel_binding(
        &self,
        session_id: &str,
        channel: &AcpBoundChannel,
    ) -> Result<()> {
        let bridge = self
            .im_bridge
            .as_ref()
            .context("ACP binding requires an IM bridge")?;
        bridge
            .acp_binding_upsert(AcpBindingUpsertRequest {
                session_id: session_id.to_string(),
                platform: channel.platform.clone(),
                channel_id: channel.channel_id.clone(),
            })
            .await
            .context("failed to upsert ACP channel binding")
    }

    pub async fn run_prepared_tool_turn(
        &self,
        prepared: PreparedToolTurn,
    ) -> Result<AcpToolResponse> {
        let started = Instant::now();
        tracing::info!(
            acp_session_id = %prepared.session_id,
            sub_session_id = %prepared.sub_session_id,
            message_len = prepared.message.len(),
            prompt_len = prepared.prompt.len(),
            "acp.run.start"
        );
        let invocation = match self
            .invoke_remote(
                &prepared.session_id,
                prepared.external_session_id.as_deref(),
                &prepared.message,
                &prepared.prompt,
                &prepared.startup_args,
                None,
            )
            .await
        {
            Ok(invocation) => invocation,
            Err(err) => {
                tracing::warn!(
                    acp_session_id = %prepared.session_id,
                    sub_session_id = %prepared.sub_session_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %err,
                    "acp.run.failed"
                );
                return Err(err).with_context(|| {
                    format!("ACP request failed for session {}", prepared.session_id)
                });
            }
        };
        let AcpInvocationResult {
            reply,
            external_session_id,
        } = invocation;
        let final_summary = reply.trim().to_string();
        let response = AcpToolResponse {
            session_id: prepared.session_id.clone(),
            reply: reply.clone(),
            final_summary: final_summary.clone(),
        };
        self.persist_turn(
            &prepared.session_id,
            &prepared.message,
            &reply,
            &final_summary,
            external_session_id.as_deref(),
        )
        .await?;
        tracing::info!(
            acp_session_id = %prepared.session_id,
            sub_session_id = %prepared.sub_session_id,
            reply_len = reply.len(),
            final_summary_len = final_summary.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "acp.run.completed"
        );
        Ok(response)
    }

    async fn run_prepared_streaming_tool_turn(
        &self,
        prepared: PreparedToolTurn,
        event_tx: Option<mpsc::UnboundedSender<AcpToolTaskEvent>>,
        _decision_rx: Option<mpsc::UnboundedReceiver<AcpApprovalDecision>>,
    ) -> Result<AcpToolResponse> {
        let started = Instant::now();
        tracing::info!(
            acp_session_id = %prepared.session_id,
            sub_session_id = %prepared.sub_session_id,
            message_len = prepared.message.len(),
            prompt_len = prepared.prompt.len(),
            "acp.run.start"
        );
        let reply = match &self.config {
            AcpConfig::Local {
                client: AcpRemoteClient::Remi,
                local_args,
                local_bin,
                ..
            } => {
                if let Some(local_bin) = local_bin {
                    invoke_local_acp_stdio(
                        local_bin,
                        &prepared.session_id,
                        prepared.external_session_id.as_deref(),
                        &prepared.prompt,
                        local_args,
                        event_tx,
                    )
                    .await
                } else {
                    anyhow::bail!(
                        "internal Remi ACP runner was not routed to the in-process spawn path"
                    )
                }
            }
            AcpConfig::Local {
                client: AcpRemoteClient::Other(_),
                local_args,
                local_bin,
                ..
            } => {
                if !prepared.startup_args.is_empty() {
                    anyhow::bail!(
                        "startup_args are not supported by the ACP backend; configure local adapter args in the ACP profile"
                    );
                }
                let local_bin = local_bin
                    .as_deref()
                    .context("REMI_ACP_LOCAL_BIN is not configured")?;
                invoke_local_acp_stdio(
                    local_bin,
                    &prepared.session_id,
                    prepared.external_session_id.as_deref(),
                    &prepared.prompt,
                    local_args,
                    event_tx,
                )
                .await
            }
            AcpConfig::Remote {
                base_url,
                client,
                agent_name,
                api_key,
                model,
            } => invoke_remote_http(
                base_url,
                client,
                agent_name,
                api_key.as_deref(),
                model.as_deref(),
                &prepared.session_id,
                &prepared.prompt,
            )
            .await
            .map(AcpInvocationResult::new),
        };
        let invocation = match reply {
            Ok(invocation) => invocation,
            Err(err) => {
                tracing::warn!(
                    acp_session_id = %prepared.session_id,
                    sub_session_id = %prepared.sub_session_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %err,
                    "acp.run.failed"
                );
                return Err(err).with_context(|| {
                    format!("ACP request failed for session {}", prepared.session_id)
                });
            }
        };
        let AcpInvocationResult {
            reply,
            external_session_id,
        } = invocation;
        let final_summary = reply.trim().to_string();
        let response = AcpToolResponse {
            session_id: prepared.session_id.clone(),
            reply: reply.clone(),
            final_summary: final_summary.clone(),
        };
        self.persist_turn(
            &prepared.session_id,
            &prepared.message,
            &reply,
            &final_summary,
            external_session_id.as_deref(),
        )
        .await?;
        tracing::info!(
            acp_session_id = %prepared.session_id,
            sub_session_id = %prepared.sub_session_id,
            reply_len = reply.len(),
            final_summary_len = final_summary.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "acp.run.completed"
        );
        Ok(response)
    }

    pub async fn spawn_prepared_tool_turn(
        self: &Arc<Self>,
        prepared: PreparedToolTurn,
        wait_ms: u64,
    ) -> AcpToolTaskStatus {
        let spawned = self.spawn_prepared_tool_turn_with_events(prepared).await;
        if wait_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
        }
        self.poll_tool_task(&spawned.task_id).await
    }

    pub async fn spawn_prepared_tool_turn_with_events(
        self: &Arc<Self>,
        prepared: PreparedToolTurn,
    ) -> AcpSpawnedToolTask {
        let task_id = Uuid::new_v4().to_string();
        let session_id = prepared.session_id.clone();
        let sub_session_id = prepared.sub_session_id.clone();
        let (event_tx, events) = mpsc::unbounded_channel();
        let (decision_tx, decision_rx) = if self.approval_decisions_supported() {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let backend = Arc::clone(self);
        let handle = if self.uses_inprocess_local_runner() {
            drop(event_tx);
            tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current()
                    .block_on(async move { backend.run_prepared_tool_turn(prepared).await })
            })
        } else {
            tokio::spawn(async move {
                backend
                    .run_prepared_streaming_tool_turn(prepared, Some(event_tx), decision_rx)
                    .await
            })
        };
        self.tool_tasks.lock().await.insert(
            task_id.clone(),
            AcpToolTask {
                session_id: session_id.clone(),
                sub_session_id: sub_session_id.clone(),
                handle,
            },
        );

        AcpSpawnedToolTask {
            task_id,
            session_id,
            sub_session_id,
            events,
            decisions: decision_tx,
        }
    }

    fn approval_decisions_supported(&self) -> bool {
        false
    }

    fn uses_inprocess_local_runner(&self) -> bool {
        matches!(
            &self.config,
            AcpConfig::Local {
                client: AcpRemoteClient::Remi,
                local_bin: None,
                ..
            }
        )
    }

    pub async fn poll_tool_task(&self, task_id: &str) -> AcpToolTaskStatus {
        let mut tasks = self.tool_tasks.lock().await;
        let Some(task) = tasks.get(task_id) else {
            return AcpToolTaskStatus {
                status: "not_found".to_string(),
                task_id: task_id.to_string(),
                session_id: String::new(),
                sub_session_id: String::new(),
                poll_hint: None,
                response: None,
                error: Some("ACP task not found; it may have already been collected or the process restarted".to_string()),
            };
        };
        if !task.handle.is_finished() {
            return AcpToolTaskStatus {
                status: "running".to_string(),
                task_id: task_id.to_string(),
                session_id: task.session_id.clone(),
                sub_session_id: task.sub_session_id.clone(),
                poll_hint: Some(format!(
                    "Call the ACP tool again with task_id={task_id} and action=poll."
                )),
                response: None,
                error: None,
            };
        }

        let task = tasks
            .remove(task_id)
            .expect("finished ACP task should still be present");
        drop(tasks);

        match task.handle.await {
            Ok(Ok(response)) => AcpToolTaskStatus {
                status: "completed".to_string(),
                task_id: task_id.to_string(),
                session_id: task.session_id,
                sub_session_id: task.sub_session_id,
                poll_hint: None,
                response: Some(response),
                error: None,
            },
            Ok(Err(err)) => AcpToolTaskStatus {
                status: "failed".to_string(),
                task_id: task_id.to_string(),
                session_id: task.session_id,
                sub_session_id: task.sub_session_id,
                poll_hint: None,
                response: None,
                error: Some(err.to_string()),
            },
            Err(err) => AcpToolTaskStatus {
                status: "failed".to_string(),
                task_id: task_id.to_string(),
                session_id: task.session_id,
                sub_session_id: task.sub_session_id,
                poll_hint: None,
                response: None,
                error: Some(format!("ACP task failed to join: {err}")),
            },
        }
    }

    pub async fn continue_bound_session(&self, session_id: &str, message: &str) -> Result<String> {
        let prepared = self
            .prepare_tool_turn(AcpToolRequest {
                message: message.to_string(),
                session_id: Some(session_id.to_string()),
                title: None,
                current_channel: None,
                startup_args: Vec::new(),
            })
            .await?;
        let response = self.run_prepared_tool_turn(prepared).await?;
        Ok(response.reply)
    }

    pub async fn abort_tool_task(&self, task_id: &str) -> bool {
        let Some(task) = self.tool_tasks.lock().await.remove(task_id) else {
            return false;
        };
        task.handle.abort();
        true
    }

    pub async fn session_id_for_sub_session(&self, sub_session_id: &str) -> Option<String> {
        let store = self.store.lock().await;
        store
            .sessions
            .values()
            .find(|record| record.sub_session_id == sub_session_id)
            .map(|record| record.session_id.clone())
    }

    pub async fn binding_status_text(&self, platform: &str, channel_id: &str) -> Result<String> {
        let store = self.store.lock().await;
        let Some(record) = store.sessions.values().find(|record| {
            record
                .bound_channel
                .as_ref()
                .map(|channel| channel.platform == platform && channel.channel_id == channel_id)
                .unwrap_or(false)
        }) else {
            return Ok("当前频道没有绑定 ACP session。".to_string());
        };

        Ok(format!(
            "ACP 已绑定。\nsession_id: {}\nsub_session_id: {}\nlast_active_at: {}\nlast_summary: {}",
            record.session_id,
            record.sub_session_id,
            record.updated_at,
            record
                .last_summary
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("none")
        ))
    }

    pub async fn unbind_channel(&self, platform: &str, channel_id: &str) -> Result<bool> {
        let mut store = self.store.lock().await;
        let mut removed = false;
        for record in store.sessions.values_mut() {
            let matches = record
                .bound_channel
                .as_ref()
                .map(|channel| channel.platform == platform && channel.channel_id == channel_id)
                .unwrap_or(false);
            if matches {
                record.bound_channel = None;
                removed = true;
            }
        }
        if removed {
            save_store(&self.store_path, &store)?;
            if let Some(bridge) = &self.im_bridge {
                bridge
                    .acp_binding_delete(AcpBindingDeleteRequest {
                        platform: platform.to_string(),
                        channel_id: channel_id.to_string(),
                    })
                    .await
                    .context("failed to delete ACP channel binding")?;
            }
        }
        Ok(removed)
    }

    async fn persist_turn(
        &self,
        session_id: &str,
        user_message: &str,
        assistant_reply: &str,
        summary: &str,
        external_session_id: Option<&str>,
    ) -> Result<()> {
        let mut store = self.store.lock().await;
        let record = store
            .sessions
            .get_mut(session_id)
            .with_context(|| format!("ACP session not found while persisting: {session_id}"))?;
        let now = Utc::now().to_rfc3339();
        record.updated_at = now.clone();
        if let Some(external_session_id) = external_session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            record.external_session_id = Some(external_session_id.to_string());
        }
        record.last_summary = Some(summary.to_string());
        record.transcript.push(AcpTurnRecord {
            user_message: user_message.to_string(),
            assistant_reply: assistant_reply.to_string(),
            summary: summary.to_string(),
            created_at: now,
        });
        save_store(&self.store_path, &store)
    }

    async fn invoke_remote(
        &self,
        session_id: &str,
        external_session_id: Option<&str>,
        message: &str,
        prompt: &str,
        startup_args: &[String],
        event_tx: Option<mpsc::UnboundedSender<AcpToolTaskEvent>>,
    ) -> Result<AcpInvocationResult> {
        match &self.config {
            AcpConfig::Local {
                client,
                agent_name,
                local_args,
                local_bin,
            } => match client {
                AcpRemoteClient::Remi => {
                    if let Some(local_bin) = local_bin {
                        tracing::info!(
                            acp_session_id = session_id,
                            acp_client = "remi",
                            configured_startup_args_count = local_args.len(),
                            "acp.invoke.external_stdio.start"
                        );
                        return invoke_local_acp_stdio(
                            local_bin,
                            session_id,
                            external_session_id,
                            prompt,
                            local_args,
                            event_tx,
                        )
                        .await;
                    }
                    tracing::info!(
                        acp_session_id = session_id,
                        acp_client = "remi",
                        local_runner = self
                            .local_runner
                            .read()
                            .map(|slot| slot.is_some())
                            .unwrap_or(false),
                        "acp.invoke.start"
                    );
                    let runner = self
                        .local_runner
                        .read()
                        .ok()
                        .and_then(|slot| slot.as_ref().map(Arc::clone));
                    if let Some(runner) = runner {
                        runner
                            .run(session_id, message)
                            .await
                            .map(AcpInvocationResult::new)
                    } else {
                        anyhow::bail!(
                            "local Remi ACP runner is not installed for agent `{agent_name}`"
                        )
                    }
                }
                AcpRemoteClient::Other(client_name) => {
                    tracing::info!(
                        acp_session_id = session_id,
                        acp_client = %client_name,
                        prompt_len = prompt.len(),
                        configured_startup_args_count = local_args.len(),
                        "acp.invoke.start"
                    );
                    if !startup_args.is_empty() {
                        anyhow::bail!(
                            "startup_args are not supported by the ACP backend; configure local adapter args in the ACP profile"
                        );
                    }
                    let local_bin = local_bin
                        .as_deref()
                        .context("REMI_ACP_LOCAL_BIN is not configured")?;
                    invoke_local_acp_stdio(
                        local_bin,
                        session_id,
                        external_session_id,
                        prompt,
                        local_args,
                        event_tx,
                    )
                    .await
                }
            },
            AcpConfig::Remote {
                base_url,
                client,
                agent_name,
                api_key,
                model,
            } => invoke_remote_http(
                base_url,
                client,
                agent_name,
                api_key.as_deref(),
                model.as_deref(),
                session_id,
                prompt,
            )
            .await
            .map(AcpInvocationResult::new),
        }
    }
}

async fn invoke_remote_http(
    base_url: &str,
    client: &AcpRemoteClient,
    agent_name: &str,
    api_key: Option<&str>,
    model: Option<&str>,
    session_id: &str,
    prompt: &str,
) -> Result<String> {
    let endpoint = format!("{}/runs", base_url);
    tracing::info!(
        acp_session_id = session_id,
        acp_client = ?client,
        endpoint = %endpoint,
        agent_name,
        prompt_len = prompt.len(),
        "acp.remote.start"
    );
    let mut request = reqwest::Client::new()
        .post(&endpoint)
        .header(CONTENT_TYPE, "application/json");
    if let Some(api_key) = api_key {
        request = request.header(AUTHORIZATION, format!("Bearer {api_key}"));
    }

    let body = serde_json::json!({
        "agent": agent_name,
        "agent_name": agent_name,
        "model": model,
        "sessionId": session_id,
        "session_id": session_id,
        "input": [{
            "role": "user",
            "parts": [{
                "type": "text",
                "text": prompt,
            }]
        }]
    });

    let response = request
        .json(&body)
        .send()
        .await
        .context("failed to send ACP request")?
        .error_for_status()
        .context("ACP server returned error status")?;
    let json: Value = response.json().await.context("invalid ACP JSON response")?;
    let text =
        extract_text_from_response(&json).context("ACP response did not contain readable text")?;
    tracing::info!(
        acp_session_id = session_id,
        acp_client = ?client,
        endpoint = %endpoint,
        response_len = text.len(),
        "acp.remote.completed"
    );
    Ok(text)
}

fn local_acp_binary_from_env(client: &AcpRemoteClient) -> Option<String> {
    std::env::var("REMI_ACP_LOCAL_BIN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            if client.is_codex()
                && (std::env::var("REMI_ACP_CODEX_BIN")
                    .ok()
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false)
                    || std::env::var("REMI_ACP_CODEX_ARGS")
                        .ok()
                        .map(|value| !value.trim().is_empty())
                        .unwrap_or(false))
            {
                std::env::current_exe()
                    .ok()
                    .map(|path| path.to_string_lossy().to_string())
            } else {
                None
            }
        })
}

fn local_acp_binary_available(bin: &str) -> bool {
    StdCommand::new(bin)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn local_acp_args_from_env(client: &AcpRemoteClient) -> Vec<String> {
    let Ok(value) = std::env::var("REMI_ACP_LOCAL_ARGS") else {
        return legacy_codex_adapter_args_from_env(client);
    };
    let value = value.trim();
    if value.is_empty() {
        return legacy_codex_adapter_args_from_env(client);
    }
    match serde_json::from_str::<Vec<String>>(value) {
        Ok(args) => args,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "ignoring invalid REMI_ACP_LOCAL_ARGS; expected JSON string array"
            );
            Vec::new()
        }
    }
}

fn legacy_codex_adapter_args_from_env(client: &AcpRemoteClient) -> Vec<String> {
    if !client.is_codex() {
        return Vec::new();
    }
    let codex_bin = std::env::var("REMI_ACP_CODEX_BIN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let codex_args = string_array_env("REMI_ACP_CODEX_ARGS");
    if codex_bin.is_none() && codex_args.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["acp-adapter".to_string(), "codex".to_string()];
    if let Some(bin) = codex_bin {
        args.push("--bin".to_string());
        args.push(bin);
    }
    for arg in codex_args {
        args.push("--arg".to_string());
        args.push(arg);
    }
    args
}

fn string_array_env(key: &str) -> Vec<String> {
    let Ok(value) = std::env::var(key) else {
        return Vec::new();
    };
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<String>>(value) {
        Ok(args) => args,
        Err(err) => {
            tracing::warn!(
                key,
                error = %err,
                "ignoring invalid string array env; expected JSON string array"
            );
            Vec::new()
        }
    }
}

async fn invoke_local_acp_stdio(
    program: &str,
    session_id: &str,
    external_session_id: Option<&str>,
    message: &str,
    configured_startup_args: &[String],
    event_tx: Option<mpsc::UnboundedSender<AcpToolTaskEvent>>,
) -> Result<AcpInvocationResult> {
    let started = Instant::now();
    let cwd = std::env::current_dir().context("failed to resolve current directory for ACP")?;
    let mut argv = Vec::with_capacity(1 + configured_startup_args.len());
    argv.push(program.to_string());
    argv.extend(configured_startup_args.iter().cloned());
    tracing::info!(
        program,
        cwd = %cwd.display(),
        args_count = configured_startup_args.len(),
        session_id,
        message_len = message.len(),
        "acp.external_stdio.start"
    );
    let agent = AcpAgent::from_args(argv).map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let prompt_text = message.to_string();
    let remi_session_id = session_id.to_string();
    let external_session_id = external_session_id.map(str::to_string);
    let result = Client
        .builder()
        .connect_with(
            agent,
            |connection: agent_client_protocol::ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let (mut session, actual_external_session_id) =
                    if let Some(external_session_id) = external_session_id {
                        connection
                            .send_request(LoadSessionRequest::new(
                                external_session_id.clone(),
                                cwd.clone(),
                            ))
                            .block_task()
                            .await?;
                        let session = connection.attach_session(
                            NewSessionResponse::new(external_session_id.clone()),
                            Vec::new(),
                        )?;
                        (session, external_session_id)
                    } else {
                        let response = connection
                            .send_request(NewSessionRequest::new(cwd.clone()))
                            .block_task()
                            .await?;
                        let external_session_id = response.session_id.to_string();
                        let session = connection.attach_session(response, Vec::new())?;
                        (session, external_session_id)
                    };
                session.send_prompt(prompt_text)?;
                let mut output = String::new();
                loop {
                    let stopped = handle_external_acp_session_message(
                        session.read_update().await?,
                        &actual_external_session_id,
                        &mut output,
                        event_tx.as_ref(),
                    )
                    .await?;
                    if stopped {
                        while let Ok(Ok(message)) =
                            tokio::time::timeout(Duration::from_millis(50), session.read_update())
                                .await
                        {
                            let _ = handle_external_acp_session_message(
                                message,
                                &actual_external_session_id,
                                &mut output,
                                event_tx.as_ref(),
                            )
                            .await?;
                        }
                        break;
                    }
                }
                Ok(AcpInvocationResult {
                    reply: output,
                    external_session_id: Some(actual_external_session_id),
                })
            },
        )
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    let text = result.reply.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("external ACP agent completed without a readable final message");
    }
    tracing::info!(
        program,
        session_id = %remi_session_id,
        external_session_id = result.external_session_id.as_deref().unwrap_or(""),
        reply_len = text.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "acp.external_stdio.completed"
    );
    Ok(AcpInvocationResult {
        reply: text,
        external_session_id: result.external_session_id,
    })
}

async fn handle_external_acp_session_message(
    message: SessionMessage,
    expected_session_id: &str,
    output: &mut String,
    event_tx: Option<&mpsc::UnboundedSender<AcpToolTaskEvent>>,
) -> agent_client_protocol::Result<bool> {
    match message {
        SessionMessage::SessionMessage(dispatch) => {
            if let Ok(untyped) = dispatch.to_untyped_message() {
                if matches!(untyped.method(), "sessionUpdate" | "session/update") {
                    let notification: SessionNotification =
                        serde_json::from_value(untyped.params().clone())?;
                    handle_external_acp_notification(
                        notification,
                        expected_session_id,
                        output,
                        event_tx,
                    );
                    return Ok(false);
                }
            }
            let mut parsed = None;
            MatchDispatch::new(dispatch)
                .if_notification(async |notification: SessionNotification| {
                    parsed = Some(notification);
                    Ok(())
                })
                .await
                .otherwise_ignore()?;
            if let Some(notification) = parsed {
                handle_external_acp_notification(
                    notification,
                    expected_session_id,
                    output,
                    event_tx,
                );
            }
            Ok(false)
        }
        SessionMessage::StopReason(_stop_reason) => Ok(true),
        _ => Ok(false),
    }
}

fn handle_external_acp_notification(
    notification: SessionNotification,
    expected_session_id: &str,
    output: &mut String,
    event_tx: Option<&mpsc::UnboundedSender<AcpToolTaskEvent>>,
) {
    if notification.session_id.to_string() != expected_session_id {
        return;
    }
    match notification.update {
        AcpSessionUpdate::AgentMessageChunk(AcpContentChunk {
            content: AcpContentBlock::Text(text),
            ..
        }) => {
            output.push_str(&text.text);
            if let Some(tx) = event_tx {
                let _ = tx.send(AcpToolTaskEvent::Delta(text.text));
            }
        }
        AcpSessionUpdate::AgentThoughtChunk(AcpContentChunk {
            content: AcpContentBlock::Text(text),
            ..
        }) => {
            if let Some(tx) = event_tx {
                let _ = tx.send(AcpToolTaskEvent::Thinking(text.text));
            }
        }
        _ => {}
    }
}

fn load_store(path: &PathBuf) -> Result<AcpStore> {
    let raw = std::fs::read_to_string(path)?;
    serde_json::from_str(&raw).context("failed to parse ACP session store")
}

fn save_store(path: &PathBuf, store: &AcpStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let raw =
        serde_json::to_string_pretty(store).context("failed to serialize ACP session store")?;
    std::fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
}

fn build_prompt_with_recent_summaries(
    config: &AcpConfig,
    store: &AcpStore,
    current_session_id: &str,
    transcript: &[AcpTurnRecord],
    message: &str,
) -> String {
    let mut sections = vec![format!("ACP agent: {}", config.agent_name())];

    let mut recent_sessions: Vec<_> = store
        .sessions
        .values()
        .filter(|record| record.session_id != current_session_id)
        .filter_map(|record| {
            record.last_summary.as_ref().map(|summary| {
                (
                    record.updated_at.clone(),
                    record.session_id.clone(),
                    summary.clone(),
                )
            })
        })
        .collect();
    recent_sessions.sort_by(|a, b| b.0.cmp(&a.0));
    if !recent_sessions.is_empty() {
        sections.push("Recent 5 ACP session summaries:".to_string());
        for (index, (_, session_id, summary)) in recent_sessions.into_iter().take(5).enumerate() {
            sections.push(format!("{}. [{}] {}", index + 1, session_id, summary));
        }
    }

    if !transcript.is_empty() {
        sections.push("Current ACP session transcript:".to_string());
        for turn in transcript {
            sections.push(format!("User: {}", turn.user_message));
            sections.push(format!("Assistant: {}", turn.assistant_reply));
        }
    }

    sections.push("Current user message:".to_string());
    sections.push(message.to_string());
    sections.join("\n\n")
}

#[cfg(test)]
#[derive(Debug, Default)]
struct LocalStubPromptContext {
    current_message: String,
    transcript: Vec<(String, String)>,
    recent_summaries: Vec<String>,
}

#[cfg(test)]
fn invoke_local_stub(agent_name: &str, session_id: &str, prompt: &str) -> String {
    let context = parse_local_stub_prompt(prompt);
    let transcript_turns = context.transcript.len();
    let recent_sessions = context.recent_summaries.len();
    let reply = if is_history_question(&context.current_message) {
        summarize_local_history(&context)
    } else if transcript_turns > 0 {
        format!(
            "这是本地 ACP stub。这个会话里我看到了 {} 轮之前的对话；你刚刚说的是：{}",
            transcript_turns, context.current_message
        )
    } else {
        format!(
            "这是本地 ACP stub。当前还是新会话，我先记下你的消息：{}",
            context.current_message
        )
    };
    format!(
        "[local-acp:{agent_name}] session={session_id} transcript_turns={transcript_turns} recent_sessions={recent_sessions}\nReply: {reply}"
    )
}

#[cfg(test)]
fn parse_local_stub_prompt(prompt: &str) -> LocalStubPromptContext {
    let mut context = LocalStubPromptContext::default();
    let current_split = "\n\nCurrent user message:\n";
    let transcript_split = "\n\nCurrent ACP session transcript:\n";
    let recent_split = "\n\nRecent 5 ACP session summaries:\n";

    context.current_message = prompt
        .split(current_split)
        .last()
        .unwrap_or(prompt)
        .trim()
        .to_string();

    if let Some((before_current, _)) = prompt.split_once(current_split) {
        if let Some((_, after_recent)) = before_current.split_once(recent_split) {
            let recent_section = after_recent
                .split_once(transcript_split)
                .map(|(recent_only, _)| recent_only)
                .unwrap_or(after_recent);
            context.recent_summaries = recent_section
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .filter(|line| line.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
                .map(ToOwned::to_owned)
                .collect();
        }

        if let Some((_, transcript_section)) = before_current.split_once(transcript_split) {
            context.transcript = parse_transcript_pairs(transcript_section);
        }
    }

    context
}

#[cfg(test)]
fn parse_transcript_pairs(section: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut current_user: Option<String> = None;

    for line in section
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if let Some(rest) = line.strip_prefix("User: ") {
            current_user = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Assistant: ") {
            if let Some(user) = current_user.take() {
                pairs.push((user, rest.trim().to_string()));
            }
        }
    }

    pairs
}

#[cfg(test)]
fn is_history_question(message: &str) -> bool {
    let normalized = message.trim().to_ascii_lowercase();
    [
        "之前聊了啥",
        "之前聊了什么",
        "回顾",
        "总结一下",
        "历史",
        "上次聊",
        "之前说过",
        "what did we talk",
        "what did we discuss",
        "recap",
        "summary",
        "history",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

#[cfg(test)]
fn summarize_local_history(context: &LocalStubPromptContext) -> String {
    if context.transcript.is_empty() {
        return "这是本地 ACP stub。这个会话里还没有可回顾的历史消息。".to_string();
    }

    let highlights = context
        .transcript
        .iter()
        .take(3)
        .enumerate()
        .map(|(index, (user, assistant))| {
            format!(
                "{}. 你说过：{}；我当时回复：{}",
                index + 1,
                user,
                first_sentence(assistant)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        "这是本地 ACP stub。这个会话里之前主要有 {} 轮对话。{}",
        context.transcript.len(),
        highlights
    )
}

#[cfg(test)]
fn first_sentence(text: &str) -> String {
    let trimmed = text.trim();
    let cutoff = ['\n', '。', '！', '？', '.', '!', '?']
        .iter()
        .filter_map(|separator| trimmed.find(*separator))
        .min()
        .unwrap_or(trimmed.len());
    trimmed[..cutoff].trim().to_string()
}

fn extract_text_from_response(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    if let Some(object) = value.as_object() {
        for key in ["output_text", "text", "reply", "content", "result"] {
            if let Some(text) = object.get(key).and_then(extract_text_from_response) {
                return Some(text);
            }
        }
        if let Some(parts) = object.get("parts").and_then(Value::as_array) {
            let joined = parts
                .iter()
                .filter_map(extract_text_from_response)
                .collect::<Vec<_>>()
                .join("\n");
            if !joined.trim().is_empty() {
                return Some(joined);
            }
        }
    }

    if let Some(array) = value.as_array() {
        let joined = array
            .iter()
            .filter_map(extract_text_from_response)
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.trim().is_empty() {
            return Some(joined);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{
        invoke_local_acp_stdio, invoke_local_stub, AcpBackend, AcpConfig, AcpRemoteClient,
        AcpToolRequest, AcpToolTaskEvent,
    };
    use anyhow::Result;
    use std::future::Future;
    use std::io::Write;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    static ACP_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_acp_client_env<T>(client: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ACP_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = [
            "REMI_ACP_CLIENT",
            "REMI_ACP_LOCAL_BIN",
            "REMI_ACP_LOCAL_ARGS",
            "REMI_ACP_CODEX_BIN",
            "REMI_ACP_CODEX_ARGS",
        ]
        .into_iter()
        .map(|key| (key, std::env::var(key).ok()))
        .collect::<Vec<_>>();
        unsafe {
            match client {
                Some(client) => std::env::set_var("REMI_ACP_CLIENT", client),
                None => std::env::remove_var("REMI_ACP_CLIENT"),
            }
            std::env::remove_var("REMI_ACP_LOCAL_BIN");
            std::env::remove_var("REMI_ACP_LOCAL_ARGS");
            std::env::remove_var("REMI_ACP_CODEX_BIN");
            std::env::remove_var("REMI_ACP_CODEX_ARGS");
        }
        let result = f();
        unsafe {
            for (key, value) in previous {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        result
    }

    fn with_acp_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let _guard = ACP_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = vars
            .iter()
            .map(|(key, _)| (*key, std::env::var(key).ok()))
            .collect::<Vec<_>>();
        unsafe {
            for (key, value) in vars {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        let result = f();
        unsafe {
            for (key, value) in previous {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        result
    }

    struct EchoLocalRunner;

    impl super::AcpLocalRunner for EchoLocalRunner {
        fn run<'a>(
            &'a self,
            session_id: &'a str,
            message: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + 'a>> {
            Box::pin(async move { Ok(format!("runner session={session_id} message={message}")) })
        }
    }

    #[tokio::test]
    async fn local_backend_creates_and_resumes_session() {
        let dir = tempdir().unwrap();
        let backend = with_acp_client_env(Some("remi"), || {
            AcpBackend::new(dir.path().to_path_buf(), None)
        });
        backend.set_local_runner(Arc::new(EchoLocalRunner));

        let prepared = backend
            .prepare_tool_turn(AcpToolRequest {
                message: "hello acp".to_string(),
                session_id: None,
                title: Some("demo".to_string()),
                current_channel: None,
                startup_args: Vec::new(),
            })
            .await
            .unwrap();
        let first_session = prepared.session_id.clone();
        let first = backend.run_prepared_tool_turn(prepared).await.unwrap();
        assert_eq!(first.session_id, first_session);
        assert!(first.reply.contains("hello acp"));

        let prepared = backend
            .prepare_tool_turn(AcpToolRequest {
                message: "resume me".to_string(),
                session_id: Some(first_session.clone()),
                title: None,
                current_channel: None,
                startup_args: Vec::new(),
            })
            .await
            .unwrap();
        let second = backend.run_prepared_tool_turn(prepared).await.unwrap();
        assert_eq!(second.session_id, first_session);
        assert!(second.reply.contains("resume me"));
    }

    #[tokio::test]
    async fn local_backend_uses_injected_runner() {
        let dir = tempdir().unwrap();
        let backend = with_acp_client_env(Some("remi"), || {
            AcpBackend::new(dir.path().to_path_buf(), None)
        });
        backend.set_local_runner(Arc::new(EchoLocalRunner));

        let prepared = backend
            .prepare_tool_turn(AcpToolRequest {
                message: "use real runner".to_string(),
                session_id: None,
                title: Some("runner test".to_string()),
                current_channel: None,
                startup_args: Vec::new(),
            })
            .await
            .unwrap();
        let session_id = prepared.session_id.clone();
        let response = backend.run_prepared_tool_turn(prepared).await.unwrap();

        assert_eq!(response.session_id, session_id);
        assert!(response.reply.contains("runner session="));
        assert!(response.reply.contains("message=use real runner"));
        assert!(!response.reply.contains("[local-acp:"));
    }

    #[tokio::test]
    async fn local_backend_spawn_uses_injected_runner() {
        let dir = tempdir().unwrap();
        let backend = with_acp_client_env(Some("remi"), || {
            Arc::new(AcpBackend::new(dir.path().to_path_buf(), None))
        });
        backend.set_local_runner(Arc::new(EchoLocalRunner));

        let prepared = backend
            .prepare_tool_turn(AcpToolRequest {
                message: "spawn runner".to_string(),
                session_id: None,
                title: Some("spawn runner test".to_string()),
                current_channel: None,
                startup_args: Vec::new(),
            })
            .await
            .unwrap();
        let spawned = backend.spawn_prepared_tool_turn_with_events(prepared).await;
        let completed = loop {
            let status = backend.poll_tool_task(&spawned.task_id).await;
            if status.status != "running" {
                break status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(completed.status, "completed");
        let response = completed
            .response
            .expect("completed task should include response");
        assert!(response.reply.contains("message=spawn runner"));
    }

    #[tokio::test]
    async fn startup_args_are_rejected_by_backend() {
        let _guard = ACP_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe {
            std::env::set_var("REMI_ACP_MODE", "remote");
            std::env::set_var("REMI_ACP_CLIENT", "codex");
            std::env::set_var("REMI_ACP_BASE_URL", "http://127.0.0.1:9");
        }
        let dir = tempdir().unwrap();
        let backend = AcpBackend::new(dir.path().to_path_buf(), None);
        let err = backend
            .prepare_tool_turn(AcpToolRequest {
                message: "hello".to_string(),
                session_id: None,
                title: None,
                current_channel: None,
                startup_args: vec!["--config".to_string()],
            })
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("startup_args are not supported by the ACP backend"));
        unsafe {
            std::env::remove_var("REMI_ACP_MODE");
            std::env::remove_var("REMI_ACP_CLIENT");
            std::env::remove_var("REMI_ACP_BASE_URL");
        }
    }

    #[test]
    fn local_stub_extracts_current_message() {
        let prompt = "ACP agent: default\n\nCurrent ACP session transcript:\n\nUser: a\nAssistant: b\n\nCurrent user message:\nhello world";
        let reply = invoke_local_stub("default", "session-1", prompt);
        assert!(reply.contains("hello world"));
        assert!(reply.contains("session=session-1"));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn local_external_acp_stdio_invokes_standard_protocol() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = ACP_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempdir().unwrap();
        let bin_path = dir.path().join("fake-acp-agent.py");
        let mut file = std::fs::File::create(&bin_path).unwrap();
        writeln!(
            file,
            r#"#!/usr/bin/env python3
import json
import sys

for raw in sys.stdin:
    msg = json.loads(raw)
    method = msg.get("method")
    request_id = msg.get("id")
    params = msg.get("params") or {{}}
    if method == "initialize":
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{
                "protocolVersion": params.get("protocolVersion", 1),
                "agentCapabilities": {{"loadSession": True}},
            }},
        }}), flush=True)
    elif method == "session/new":
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{"sessionId": "external-session-1"}},
        }}), flush=True)
    elif method == "session/prompt":
        session_id = params["sessionId"]
        text = ""
        for block in params.get("prompt", []):
            if block.get("type") == "text":
                text += block.get("text", "")
        print(json.dumps({{
            "jsonrpc": "2.0",
            "method": "sessionUpdate",
            "params": {{
                "sessionId": session_id,
                "update": {{
                    "sessionUpdate": "agent_message_chunk",
                    "content": {{"type": "text", "text": "external saw: " + text}},
                }},
            }},
        }}), flush=True)
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{"stopReason": "end_turn"}},
        }}), flush=True)
"#,
        )
        .unwrap();
        drop(file);
        let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin_path, perms).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let program = bin_path.to_string_lossy().to_string();
        let result = invoke_local_acp_stdio(
            &program,
            "acp-session-1",
            None,
            "hello stdio",
            &[],
            Some(tx),
        )
        .await
        .unwrap();

        assert_eq!(result.reply, "external saw: hello stdio");
        assert_eq!(
            result.external_session_id.as_deref(),
            Some("external-session-1")
        );
        assert!(
            matches!(rx.recv().await.unwrap(), AcpToolTaskEvent::Delta(text) if text == "external saw: hello stdio")
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn local_external_acp_persists_external_session_id_for_resume() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = ACP_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempdir().unwrap();
        let bin_path = dir.path().join("fake-acp-agent.py");
        let log_path = dir.path().join("acp.log");
        let mut file = std::fs::File::create(&bin_path).unwrap();
        writeln!(
            file,
            r#"#!/usr/bin/env python3
import json
import sys

log_path = sys.argv[1]

def log(line):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(line + "\n")

for raw in sys.stdin:
    msg = json.loads(raw)
    method = msg.get("method")
    request_id = msg.get("id")
    params = msg.get("params") or {{}}
    if method == "initialize":
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{
                "protocolVersion": params.get("protocolVersion", 1),
                "agentCapabilities": {{"loadSession": True}},
            }},
        }}), flush=True)
    elif method == "session/new":
        log("session/new")
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{"sessionId": "external-session-resume"}},
        }}), flush=True)
    elif method == "session/load":
        log("session/load:" + params["sessionId"])
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{}},
        }}), flush=True)
    elif method == "session/prompt":
        session_id = params["sessionId"]
        log("session/prompt:" + session_id)
        text = ""
        for block in params.get("prompt", []):
            if block.get("type") == "text":
                text += block.get("text", "")
        print(json.dumps({{
            "jsonrpc": "2.0",
            "method": "sessionUpdate",
            "params": {{
                "sessionId": session_id,
                "update": {{
                    "sessionUpdate": "agent_message_chunk",
                    "content": {{"type": "text", "text": "resume external saw: " + text}},
                }},
            }},
        }}), flush=True)
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{"stopReason": "end_turn"}},
        }}), flush=True)
"#,
        )
        .unwrap();
        drop(file);
        let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin_path, perms).unwrap();

        unsafe {
            std::env::set_var("REMI_ACP_CLIENT", "codex");
            std::env::set_var("REMI_ACP_MODE", "local");
            std::env::set_var("REMI_ACP_LOCAL_BIN", &bin_path);
            std::env::set_var(
                "REMI_ACP_LOCAL_ARGS",
                serde_json::json!([log_path.to_string_lossy()]).to_string(),
            );
        }

        let backend = AcpBackend::new(dir.path().join("store"), None);
        let prepared = backend
            .prepare_tool_turn(AcpToolRequest {
                message: "first external".to_string(),
                session_id: None,
                title: Some("external resume".to_string()),
                current_channel: None,
                startup_args: Vec::new(),
            })
            .await
            .unwrap();
        let remi_session_id = prepared.session_id.clone();
        backend.run_prepared_tool_turn(prepared).await.unwrap();

        let prepared = backend
            .prepare_tool_turn(AcpToolRequest {
                message: "second external".to_string(),
                session_id: Some(remi_session_id.clone()),
                title: None,
                current_channel: None,
                startup_args: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(
            prepared.external_session_id.as_deref(),
            Some("external-session-resume")
        );
        backend.run_prepared_tool_turn(prepared).await.unwrap();

        let log = std::fs::read_to_string(&log_path).unwrap();
        let lines = log.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "session/new");
        assert_eq!(lines[1], "session/prompt:external-session-resume");
        assert_eq!(lines[2], "session/load:external-session-resume");
        assert_eq!(lines[3], "session/prompt:external-session-resume");
        assert!(!log.contains(&format!("session/load:{remi_session_id}")));

        unsafe {
            std::env::remove_var("REMI_ACP_CLIENT");
            std::env::remove_var("REMI_ACP_MODE");
            std::env::remove_var("REMI_ACP_LOCAL_BIN");
            std::env::remove_var("REMI_ACP_LOCAL_ARGS");
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn local_external_acp_task_returns_running_and_poll_completes() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = ACP_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempdir().unwrap();
        let bin_path = dir.path().join("fake-acp-agent.py");
        let mut file = std::fs::File::create(&bin_path).unwrap();
        writeln!(
            file,
            r#"#!/usr/bin/env python3
import json
import sys
import time

if len(sys.argv) > 1 and sys.argv[1] == "--version":
    print("fake-acp-agent 1.0")
    raise SystemExit(0)

for raw in sys.stdin:
    msg = json.loads(raw)
    method = msg.get("method")
    request_id = msg.get("id")
    params = msg.get("params") or {{}}
    if method == "initialize":
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{
                "protocolVersion": params.get("protocolVersion", 1),
                "agentCapabilities": {{"loadSession": True}},
            }},
        }}), flush=True)
    elif method == "session/new":
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{"sessionId": "slow-external-session-1"}},
        }}), flush=True)
    elif method == "session/prompt":
        time.sleep(0.1)
        session_id = params["sessionId"]
        text = ""
        for block in params.get("prompt", []):
            if block.get("type") == "text":
                text += block.get("text", "")
        print(json.dumps({{
            "jsonrpc": "2.0",
            "method": "sessionUpdate",
            "params": {{
                "sessionId": session_id,
                "update": {{
                    "sessionUpdate": "agent_message_chunk",
                    "content": {{"type": "text", "text": "slow external saw: " + text}},
                }},
            }},
        }}), flush=True)
        print(json.dumps({{
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {{"stopReason": "end_turn"}},
        }}), flush=True)
"#
        )
        .unwrap();
        drop(file);
        let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin_path, perms).unwrap();

        unsafe {
            std::env::set_var("REMI_ACP_CLIENT", "codex");
            std::env::set_var("REMI_ACP_MODE", "local");
            std::env::set_var("REMI_ACP_LOCAL_BIN", &bin_path);
        }

        let backend = Arc::new(AcpBackend::new(dir.path().to_path_buf(), None));
        let prepared = backend
            .prepare_tool_turn(AcpToolRequest {
                message: "hello slow external".to_string(),
                session_id: None,
                title: Some("slow external".to_string()),
                current_channel: None,
                startup_args: Vec::new(),
            })
            .await
            .unwrap();
        let started = backend.spawn_prepared_tool_turn(prepared, 1).await;
        assert_eq!(started.status, "running");
        assert!(started
            .poll_hint
            .as_deref()
            .unwrap()
            .contains("action=poll"));

        let mut completed = None;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let status = backend.poll_tool_task(&started.task_id).await;
            if status.status == "completed" {
                completed = Some(status);
                break;
            }
            assert_eq!(status.status, "running");
        }
        let completed = completed.expect("ACP task should complete");
        let response = completed
            .response
            .expect("completed task should include response");
        assert!(response.reply.contains("slow external saw: ACP agent"));
        assert!(response.reply.contains("hello slow external"));

        let store = backend.store.lock().await;
        let record = store.sessions.get(&response.session_id).unwrap();
        assert_eq!(record.transcript.len(), 1);
        assert_eq!(
            record.external_session_id.as_deref(),
            Some("slow-external-session-1")
        );

        unsafe {
            std::env::remove_var("REMI_ACP_CLIENT");
            std::env::remove_var("REMI_ACP_MODE");
            std::env::remove_var("REMI_ACP_LOCAL_BIN");
        }
    }

    #[test]
    fn remote_client_defaults_to_remi() {
        with_acp_client_env(None, || {
            assert_eq!(AcpRemoteClient::from_env(), AcpRemoteClient::Remi);
        });
    }

    #[test]
    fn remote_client_reads_codex_from_env() {
        with_acp_client_env(Some("codex"), || {
            assert_eq!(
                AcpRemoteClient::from_env(),
                AcpRemoteClient::Other("codex".to_string())
            );
        });
    }

    #[test]
    fn local_codex_uses_legacy_codex_env_as_adapter_config() {
        with_acp_env(
            &[
                ("REMI_ACP_CLIENT", Some("codex")),
                ("REMI_ACP_MODE", Some("local")),
                ("REMI_ACP_LOCAL_BIN", None),
                ("REMI_ACP_LOCAL_ARGS", None),
                ("REMI_ACP_CODEX_BIN", Some("/usr/local/bin/codex")),
                (
                    "REMI_ACP_CODEX_ARGS",
                    Some(r#"["--config","model=\"gpt-5-codex\""]"#),
                ),
            ],
            || {
                let config = AcpConfig::from_env();
                match config {
                    AcpConfig::Local {
                        client,
                        local_bin,
                        local_args,
                        ..
                    } => {
                        assert_eq!(client, AcpRemoteClient::Other("codex".to_string()));
                        assert!(local_bin.is_some());
                        assert_eq!(
                            local_args,
                            vec![
                                "acp-adapter".to_string(),
                                "codex".to_string(),
                                "--bin".to_string(),
                                "/usr/local/bin/codex".to_string(),
                                "--arg".to_string(),
                                "--config".to_string(),
                                "--arg".to_string(),
                                "model=\"gpt-5-codex\"".to_string(),
                            ]
                        );
                    }
                    other => panic!("expected local config, got {other:?}"),
                }
            },
        );
    }

    #[test]
    fn local_other_tool_is_unavailable_when_local_binary_is_missing() {
        with_acp_env(
            &[
                ("REMI_ACP_CLIENT", Some("codex")),
                ("REMI_ACP_MODE", Some("local")),
                (
                    "REMI_ACP_LOCAL_BIN",
                    Some("/definitely/missing/remi-cat-codex"),
                ),
            ],
            || {
                let dir = tempdir().unwrap();
                let backend = AcpBackend::new(dir.path().to_path_buf(), None);
                assert_eq!(backend.client_name(), "codex");
                assert!(!backend.active_tool_available());
            },
        );
    }

    #[test]
    #[cfg(unix)]
    fn local_other_tool_is_available_when_local_binary_runs_version() {
        let dir = tempdir().unwrap();
        let bin_path = dir.path().join("fake-acp-agent");
        std::fs::write(&bin_path, "#!/usr/bin/env sh\nprintf 'fake-acp 1.0.0\\n'\n").unwrap();
        let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&bin_path, perms).unwrap();
        let bin = bin_path.to_string_lossy().to_string();

        with_acp_env(
            &[
                ("REMI_ACP_CLIENT", Some("codex")),
                ("REMI_ACP_MODE", Some("local")),
                ("REMI_ACP_LOCAL_BIN", Some(bin.as_str())),
            ],
            || {
                let data_dir = tempdir().unwrap();
                let backend = AcpBackend::new(data_dir.path().to_path_buf(), None);
                assert_eq!(backend.client_name(), "codex");
                assert!(backend.active_tool_available());
            },
        );
    }
}
