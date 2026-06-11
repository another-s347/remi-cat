use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use uuid::Uuid;

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
    },
    Remote {
        base_url: String,
        client: AcpRemoteClient,
        agent_name: String,
        api_key: Option<String>,
        model: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpRemoteClient {
    Remi,
    Codex,
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
            "codex" => Self::Codex,
            _ => Self::Remi,
        }
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
            return Self::Local { client, agent_name };
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

        Self::Local { client, agent_name }
    }

    fn agent_name(&self) -> &str {
        match self {
            Self::Local { agent_name, .. } | Self::Remote { agent_name, .. } => agent_name.as_str(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AcpToolRequest {
    pub message: String,
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub current_channel: Option<AcpBoundChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpToolResponse {
    pub session_id: String,
    pub reply: String,
    pub final_summary: String,
}

#[derive(Debug, Clone)]
pub struct PreparedToolTurn {
    pub session_id: String,
    pub sub_session_id: String,
    pub title: Option<String>,
    pub prompt: String,
    pub message: String,
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
    im_bridge: Option<Arc<dyn ImFileBridge>>,
    local_runner: std::sync::RwLock<Option<Arc<dyn AcpLocalRunner>>>,
}

impl AcpBackend {
    pub fn new(data_dir: PathBuf, im_bridge: Option<Arc<dyn ImFileBridge>>) -> Self {
        let store_path = data_dir.join("acp").join("sessions.json");
        let store = load_store(&store_path).unwrap_or_default();
        Self {
            config: AcpConfig::from_env(),
            store_path,
            store: Mutex::new(store),
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

    pub async fn prepare_tool_turn(&self, request: AcpToolRequest) -> Result<PreparedToolTurn> {
        let started = Instant::now();
        let message = request.message.trim().to_string();
        if message.is_empty() {
            anyhow::bail!("missing ACP message");
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
            title: record.title,
            prompt,
            message,
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
        let reply = match self
            .invoke_remote(&prepared.session_id, &prepared.message, &prepared.prompt)
            .await
        {
            Ok(reply) => reply,
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

    pub async fn continue_bound_session(&self, session_id: &str, message: &str) -> Result<String> {
        let prepared = self
            .prepare_tool_turn(AcpToolRequest {
                message: message.to_string(),
                session_id: Some(session_id.to_string()),
                title: None,
                current_channel: None,
            })
            .await?;
        let response = self.run_prepared_tool_turn(prepared).await?;
        Ok(response.reply)
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
    ) -> Result<()> {
        let mut store = self.store.lock().await;
        let record = store
            .sessions
            .get_mut(session_id)
            .with_context(|| format!("ACP session not found while persisting: {session_id}"))?;
        let now = Utc::now().to_rfc3339();
        record.updated_at = now.clone();
        record.last_summary = Some(summary.to_string());
        record.transcript.push(AcpTurnRecord {
            user_message: user_message.to_string(),
            assistant_reply: assistant_reply.to_string(),
            summary: summary.to_string(),
            created_at: now,
        });
        save_store(&self.store_path, &store)
    }

    async fn invoke_remote(&self, session_id: &str, message: &str, prompt: &str) -> Result<String> {
        match &self.config {
            AcpConfig::Local {
                client, agent_name, ..
            } => match client {
                AcpRemoteClient::Remi => {
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
                        runner.run(session_id, message).await
                    } else {
                        anyhow::bail!(
                            "local Remi ACP runner is not installed for agent `{agent_name}`"
                        )
                    }
                }
                AcpRemoteClient::Codex => {
                    tracing::info!(
                        acp_session_id = session_id,
                        acp_client = "codex",
                        prompt_len = prompt.len(),
                        "acp.invoke.start"
                    );
                    invoke_local_codex(prompt).await
                }
            },
            AcpConfig::Remote {
                base_url,
                client,
                agent_name,
                api_key,
                model,
            } => {
                let endpoint = match client {
                    AcpRemoteClient::Remi => format!("{}/runs", base_url),
                    AcpRemoteClient::Codex => format!("{}/responses", base_url),
                };
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

                let body = match client {
                    AcpRemoteClient::Remi => serde_json::json!({
                        "agent": agent_name,
                        "agent_name": agent_name,
                        "sessionId": session_id,
                        "session_id": session_id,
                        "input": [{
                            "role": "user",
                            "parts": [{
                                "type": "text",
                                "text": prompt,
                            }]
                        }]
                    }),
                    AcpRemoteClient::Codex => serde_json::json!({
                        "model": model.clone().unwrap_or_else(|| "gpt-5-codex".to_string()),
                        "input": prompt,
                        "metadata": {
                            "acp_agent": agent_name,
                            "acp_session_id": session_id,
                        }
                    }),
                };

                let response = request
                    .json(&body)
                    .send()
                    .await
                    .context("failed to send ACP request")?
                    .error_for_status()
                    .context("ACP server returned error status")?;
                let json: Value = response.json().await.context("invalid ACP JSON response")?;
                let text = extract_text_from_response(&json)
                    .context("ACP response did not contain readable text")?;
                tracing::info!(
                    acp_session_id = session_id,
                    acp_client = ?client,
                    endpoint = %endpoint,
                    response_len = text.len(),
                    "acp.remote.completed"
                );
                Ok(text)
            }
        }
    }
}

async fn invoke_local_codex(prompt: &str) -> Result<String> {
    let program = std::env::var("REMI_ACP_CODEX_BIN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "codex".to_string());
    invoke_local_codex_with_program(&program, prompt).await
}

async fn invoke_local_codex_with_program(program: &str, prompt: &str) -> Result<String> {
    let started = Instant::now();
    let output_path = std::env::temp_dir().join(format!("remi-acp-codex-{}.txt", Uuid::new_v4()));
    let cwd = std::env::current_dir().context("failed to resolve current directory for Codex")?;
    tracing::info!(
        program,
        cwd = %cwd.display(),
        output_path = %output_path.display(),
        prompt_len = prompt.len(),
        "acp.codex.start"
    );
    let mut child = Command::new(program)
        .arg("exec")
        .arg("--json")
        .arg("--cd")
        .arg(&cwd)
        .arg("--sandbox")
        .arg("workspace-write")
        .arg("--output-last-message")
        .arg(&output_path)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn local Codex binary `{program}`"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write ACP prompt to Codex stdin")?;
    }

    let output = child
        .wait_with_output()
        .await
        .context("failed waiting for local Codex")?;
    let final_message = tokio::fs::read_to_string(&output_path).await.ok();
    let _ = tokio::fs::remove_file(&output_path).await;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            program,
            cwd = %cwd.display(),
            output_path = %output_path.display(),
            exit_status = %output.status,
            stdout_bytes = output.stdout.len(),
            stderr_bytes = output.stderr.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "acp.codex.failed"
        );
        anyhow::bail!(
            "local Codex exited with status {}{}{}",
            output.status,
            format_output_section("stdout", &stdout),
            format_output_section("stderr", &stderr)
        );
    }

    let text = final_message
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| extract_text_from_codex_stdout(&output.stdout))
        .context("local Codex completed without a readable final message")?;
    tracing::info!(
        program,
        cwd = %cwd.display(),
        output_path = %output_path.display(),
        exit_status = %output.status,
        stdout_bytes = output.stdout.len(),
        stderr_bytes = output.stderr.len(),
        final_message_bytes = text.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "acp.codex.completed"
    );
    Ok(text)
}

fn format_output_section(label: &str, value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("\n{label}:\n{trimmed}")
    }
}

fn extract_text_from_codex_stdout(stdout: &[u8]) -> Option<String> {
    let raw = String::from_utf8_lossy(stdout);
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| extract_text_from_response(&value))
        .last()
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
        invoke_local_codex_with_program, invoke_local_stub, AcpBackend, AcpRemoteClient,
        AcpToolRequest,
    };
    use anyhow::Result;
    use std::future::Future;
    use std::io::Write;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    static ACP_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_acp_client_env<T>(client: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ACP_ENV_LOCK.lock().unwrap();
        unsafe {
            match client {
                Some(client) => std::env::set_var("REMI_ACP_CLIENT", client),
                None => std::env::remove_var("REMI_ACP_CLIENT"),
            }
        }
        let result = f();
        unsafe {
            std::env::remove_var("REMI_ACP_CLIENT");
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

    #[test]
    fn local_stub_extracts_current_message() {
        let prompt = "ACP agent: default\n\nCurrent ACP session transcript:\n\nUser: a\nAssistant: b\n\nCurrent user message:\nhello world";
        let reply = invoke_local_stub("default", "session-1", prompt);
        assert!(reply.contains("hello world"));
        assert!(reply.contains("session=session-1"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_codex_invokes_codex_exec_and_reads_last_message() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let log_path = dir.path().join("codex.log");
        let bin_path = dir.path().join("codex");
        let mut file = std::fs::File::create(&bin_path).unwrap();
        writeln!(
            file,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" > "{log}"
prompt="$(cat)"
out=""
prev=""
for arg in "$@"; do
  if [[ "$prev" == "--output-last-message" ]]; then
    out="$arg"
    break
  fi
  prev="$arg"
done
printf 'codex saw: %s\n' "$prompt" > "$out"
printf '{{"output_text":"json fallback"}}\n'
"#,
            log = log_path.display()
        )
        .unwrap();
        drop(file);
        let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin_path, perms).unwrap();

        let reply = invoke_local_codex_with_program(
            bin_path.to_str().unwrap(),
            "ACP agent: default\n\nCurrent user message:\nhello codex",
        )
        .await
        .unwrap();

        assert!(reply.contains("codex saw: ACP agent: default"));
        assert!(reply.contains("hello codex"));
        let args = std::fs::read_to_string(log_path).unwrap();
        assert!(args.starts_with("exec --json --cd "));
        assert!(args.contains(" --sandbox workspace-write "));
        assert!(args.contains(" --output-last-message "));
        assert!(args.ends_with(" -\n"));
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
            assert_eq!(AcpRemoteClient::from_env(), AcpRemoteClient::Codex);
        });
    }
}
