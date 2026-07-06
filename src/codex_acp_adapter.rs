use std::process::Stdio as ProcessStdio;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse, MessageId,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionCapabilities,
    SessionNotification, SessionResumeCapabilities, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Stdio};
use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdout, Command};
use uuid::Uuid;

pub(crate) async fn run_codex_adapter(
    codex_bin: Option<String>,
    startup_args: Vec<String>,
) -> Result<()> {
    let codex_bin = codex_bin.unwrap_or_else(default_codex_bin);
    let local = tokio::task::LocalSet::new();
    let agent = Agent
        .builder()
        .name("remi-cat-codex-adapter")
        .on_receive_request(
            async move |initialize: InitializeRequest, responder, _cx| {
                let capabilities = AgentCapabilities::new()
                    .load_session(true)
                    .session_capabilities(
                        SessionCapabilities::new().resume(Some(SessionResumeCapabilities::new())),
                    );
                responder.respond(
                    InitializeResponse::new(initialize.protocol_version)
                        .agent_capabilities(capabilities)
                        .agent_info(Some(
                            Implementation::new(
                                "remi-cat-codex-adapter",
                                env!("CARGO_PKG_VERSION"),
                            )
                            .title(Some("remi-cat Codex adapter".to_string())),
                        )),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: NewSessionRequest, responder, _cx| {
                responder.respond(NewSessionResponse::new(Uuid::new_v4().to_string()))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: LoadSessionRequest, responder, _cx| {
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let codex_bin = codex_bin.clone();
                let startup_args = startup_args.clone();
                async move |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let session_id = request.session_id.clone();
                    let prompt = prompt_to_text(request.prompt)?;
                    run_codex_exec(
                        &codex_bin,
                        &startup_args,
                        &session_id.to_string(),
                        &prompt,
                        &cx,
                    )
                    .await
                    .map_err(|err| agent_client_protocol::util::internal_error(err.to_string()))?;
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_cancel: CancelNotification, _cx| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                message.respond_with_error(
                    agent_client_protocol::util::internal_error(
                        "unhandled Codex adapter ACP message",
                    ),
                    cx,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new());

    local
        .run_until(agent)
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

async fn run_codex_exec(
    program: &str,
    startup_args: &[String],
    session_id: &str,
    prompt: &str,
    cx: &ConnectionTo<Client>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("failed to resolve current directory for Codex")?;
    let output_path = std::env::temp_dir().join(format!("remi-acp-codex-{}.txt", Uuid::new_v4()));
    let mut command = Command::new(program);
    command
        .args(startup_args)
        .arg("exec")
        .arg("--json")
        .arg("--cd")
        .arg(&cwd)
        .arg("--sandbox")
        .arg("workspace-write")
        .arg("--output-last-message")
        .arg(&output_path)
        .arg("-")
        .stdin(ProcessStdio::piped())
        .stdout(ProcessStdio::piped())
        .stderr(ProcessStdio::piped());

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn Codex binary `{program}`"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write ACP prompt to Codex stdin")?;
    }

    let stdout = child
        .stdout
        .take()
        .context("failed to capture Codex stdout")?;
    let mut stderr = child
        .stderr
        .take()
        .context("failed to capture Codex stderr")?;
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr
            .read_to_end(&mut bytes)
            .await
            .context("failed reading Codex stderr")?;
        Ok::<_, anyhow::Error>(bytes)
    });

    let message_id = MessageId::new(Uuid::new_v4().to_string());
    let mut streamed_agent_text = false;
    let stdout = collect_codex_stdout(
        stdout,
        cx,
        session_id,
        &message_id,
        &mut streamed_agent_text,
    )
    .await?;
    let status = child.wait().await.context("failed waiting for Codex")?;
    let stderr = stderr_task
        .await
        .context("Codex stderr reader task failed")??;
    let final_message = tokio::fs::read_to_string(&output_path).await.ok();
    let _ = tokio::fs::remove_file(&output_path).await;

    if !status.success() {
        anyhow::bail!(
            "Codex exited with status {}{}{}",
            status,
            format_output_section("stdout", &String::from_utf8_lossy(&stdout)),
            format_output_section("stderr", &String::from_utf8_lossy(&stderr))
        );
    }

    let final_text = final_message
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| extract_text_from_codex_stdout(&stdout))
        .context("Codex completed without a readable final message")?;
    if !streamed_agent_text {
        send_content_chunk(
            cx,
            session_id.to_string(),
            SessionUpdate::AgentMessageChunk,
            message_id,
            final_text,
        )?;
    }
    Ok(())
}

async fn collect_codex_stdout(
    stdout: ChildStdout,
    cx: &ConnectionTo<Client>,
    session_id: &str,
    message_id: &MessageId,
    streamed_agent_text: &mut bool,
) -> Result<Vec<u8>> {
    let mut reader = BufReader::new(stdout).lines();
    let mut raw = Vec::new();
    while let Some(line) = reader
        .next_line()
        .await
        .context("failed reading Codex stdout")?
    {
        raw.extend_from_slice(line.as_bytes());
        raw.push(b'\n');
        if let Some((update, text)) = codex_stdout_line_update(&line) {
            if matches!(update, CodexLineUpdate::Agent) {
                *streamed_agent_text = true;
            }
            let update_fn = match update {
                CodexLineUpdate::Agent => SessionUpdate::AgentMessageChunk,
                CodexLineUpdate::Thought => SessionUpdate::AgentThoughtChunk,
            };
            send_content_chunk(
                cx,
                session_id.to_string(),
                update_fn,
                message_id.clone(),
                text,
            )?;
        }
    }
    Ok(raw)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexLineUpdate {
    Agent,
    Thought,
}

fn codex_stdout_line_update(line: &str) -> Option<(CodexLineUpdate, String)> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "turn.started" {
        return Some((CodexLineUpdate::Thought, "Codex turn started.".to_string()));
    }
    if event_type == "thread.started" || event_type == "turn.completed" {
        return None;
    }

    let item = value.get("item").unwrap_or(&value);
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(event_type);
    if is_tool_like_codex_item(item_type) {
        let title = item
            .get("name")
            .or_else(|| item.get("tool_name"))
            .or_else(|| item.get("command"))
            .and_then(Value::as_str)
            .unwrap_or(item_type);
        let body = extract_text_from_response(item)
            .or_else(|| serde_json::to_string(item).ok())
            .unwrap_or_else(|| item_type.to_string());
        return Some((CodexLineUpdate::Thought, format!("{title}: {body}")));
    }

    let text = extract_text_from_response(item)
        .or_else(|| value.get("delta").and_then(extract_text_from_response))
        .or_else(|| value.get("text").and_then(extract_text_from_response))?;
    if is_reasoning_like_codex_item(item_type) {
        Some((CodexLineUpdate::Thought, text))
    } else {
        Some((CodexLineUpdate::Agent, text))
    }
}

fn send_content_chunk(
    cx: &ConnectionTo<Client>,
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    update: fn(ContentChunk) -> SessionUpdate,
    message_id: MessageId,
    text: String,
) -> agent_client_protocol::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    cx.send_notification(SessionNotification::new(
        session_id,
        update(
            ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                .message_id(Some(message_id)),
        ),
    ))
}

fn prompt_to_text(blocks: Vec<ContentBlock>) -> agent_client_protocol::Result<String> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                if !text.text.trim().is_empty() {
                    parts.push(text.text);
                }
            }
            ContentBlock::ResourceLink(link) => {
                parts.push(format!("{}: {}", link.name, link.uri));
            }
            other => {
                parts.push(format!("[unsupported ACP content block: {other:?}]"));
            }
        }
    }
    let text = parts.join("\n\n");
    if text.trim().is_empty() {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("session/prompt requires at least one text or resource_link content block"));
    }
    Ok(text)
}

fn default_codex_bin() -> String {
    std::env::var("REMI_CODEX_BIN")
        .or_else(|_| std::env::var("REMI_ACP_CODEX_BIN"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "codex".to_string())
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

fn is_tool_like_codex_item(item_type: &str) -> bool {
    let item_type = item_type.to_ascii_lowercase();
    item_type.contains("tool")
        || item_type.contains("command")
        || item_type.contains("exec")
        || item_type.contains("mcp")
        || item_type.contains("patch")
}

fn is_reasoning_like_codex_item(item_type: &str) -> bool {
    let item_type = item_type.to_ascii_lowercase();
    item_type.contains("reasoning")
        || item_type.contains("thought")
        || item_type.contains("plan")
        || item_type.contains("analysis")
}

fn extract_text_from_response(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str().map(str::to_string) {
        if !text.trim().is_empty() {
            return Some(text);
        }
    }

    for key in [
        "output_text",
        "text",
        "content",
        "message",
        "delta",
        "summary",
        "result",
    ] {
        if let Some(candidate) = value.get(key).and_then(extract_text_from_response) {
            return Some(candidate);
        }
    }

    if let Some(array) = value.get("output").and_then(Value::as_array) {
        let joined = array
            .iter()
            .filter_map(extract_text_from_response)
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.trim().is_empty() {
            return Some(joined);
        }
    }

    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        let joined = choices
            .iter()
            .filter_map(|choice| {
                choice
                    .get("message")
                    .or_else(|| choice.get("delta"))
                    .and_then(extract_text_from_response)
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.trim().is_empty() {
            return Some(joined);
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
