//! Workspace-rooted filesystem tools, bash executor, and web search.
//!
//! All `Rooted*` tools sandbox every path under a root directory so absolute
//! paths like `/etc/passwd` map to `<root>/etc/passwd`.
//!
//! `WorkspaceBashTool` runs real `bash -c` with `current_dir` set to the root.
//!
//! `ExaSearchTool` calls the Exa Search API when `EXA_API_KEY` is set.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::Context;
use async_stream::stream;
use base64::Engine as _;
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    AgentError, Content, ContentPart, ResumePayload, Tool, ToolContext, ToolOutput, ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command as TokioCommand};
use tokio::sync::Mutex;

use crate::sandbox::{Sandbox, SandboxBashOutput, SandboxBashStatus};

pub const DEFAULT_FS_READ_LENGTH: usize = 32 * 1024;

const FS_READ_LAST_STATE_KEY: &str = "__fs_read_last";

// ── helper ────────────────────────────────────────────────────────────────────

fn log_preview(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in value.chars().take(max_chars) {
        out.push(ch);
    }
    if value.chars().count() > max_chars {
        out.push_str("...");
    }
    out.replace('\n', "\\n")
}

fn detect_inline_image_media_type(path: &str, bytes: &[u8]) -> Option<&'static str> {
    detect_inline_image_media_type_from_magic(bytes)
        .or_else(|| detect_inline_image_media_type_from_extension(path))
}

fn detect_inline_image_media_type_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn detect_inline_image_media_type_from_extension(path: &str) -> Option<&'static str> {
    let ext = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn inline_image_summary(path: &str, mime_type: &str, total_bytes: usize) -> String {
    format!(
        "Read image file {path} inline.\n[mime_type={mime_type} total_bytes={total_bytes}]\n[offset/length are ignored for auto-detected images]"
    )
}

// ── SecretRedactor ────────────────────────────────────────────────────────────

/// Redacts secret values from tool output text using Aho-Corasick multi-pattern
/// search.  Thread-safe; intended to be held behind `Arc<RwLock<_>>` and rebuilt
/// when the secret set changes.
pub struct SecretRedactor {
    /// Built automaton, `None` when there are no secrets.
    ac: Option<AhoCorasick>,
    /// Replacement labels — index-aligned with the AhoCorasick patterns.
    labels: Vec<String>,
}

impl SecretRedactor {
    /// Construct a redactor with no patterns (identity transform).
    pub fn empty() -> Self {
        Self {
            ac: None,
            labels: Vec::new(),
        }
    }

    /// (Re-)build from a `key → value` map.
    ///
    /// Only non-empty values are included; values shorter than 4 bytes are
    /// skipped to avoid spurious matches of very short strings.
    pub fn from_entries(entries: &HashMap<String, String>) -> Self {
        let mut patterns: Vec<&str> = Vec::new();
        let mut labels: Vec<String> = Vec::new();

        for (key, val) in entries {
            if val.len() < 4 {
                continue;
            }
            patterns.push(val);
            labels.push(key.clone());
        }

        if patterns.is_empty() {
            return Self::empty();
        }

        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(patterns)
            .expect("AhoCorasick build failed");

        Self {
            ac: Some(ac),
            labels,
        }
    }

    /// Replace all secret values in `text` with `[REDACTED:<KEY>]`.
    /// Returns the original string unchanged when no secrets are configured.
    pub fn redact(&self, text: &str) -> String {
        let Some(ac) = &self.ac else {
            return text.to_string();
        };

        let mut result = String::with_capacity(text.len());
        let mut last = 0usize;

        for m in ac.find_iter(text) {
            result.push_str(&text[last..m.start()]);
            result.push_str("[REDACTED:");
            result.push_str(&self.labels[m.pattern().as_usize()]);
            result.push(']');
            last = m.end();
        }
        result.push_str(&text[last..]);
        result
    }
}

/// Convenience alias used by `CatBot`.
pub type SharedRedactor = Arc<RwLock<SecretRedactor>>;

// ── BashMode ──────────────────────────────────────────────────────────────────

/// Controls how [`WorkspaceBashTool`] executes commands and what working
/// directory it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashMode {
    /// Local (development) mode.
    ///
    /// `cwd` is set to the agent data directory so relative paths resolve
    /// inside the workspace.  The data directory is created on demand.
    Local,

    /// Docker (production) mode.
    ///
    /// `cwd` is `/` — the full container filesystem is accessible without
    /// any path indirection.  The container filesystem is ephemeral and
    /// resets on every restart.
    Docker,
}

// ── WorkspaceBashTool ─────────────────────────────────────────────────────────

pub struct WorkspaceBashTool {
    pub sandbox: Arc<dyn Sandbox>,
    pub redactor: SharedRedactor,
}

impl WorkspaceBashTool {
    pub fn new(sandbox: Arc<dyn Sandbox>, redactor: SharedRedactor) -> Self {
        Self { sandbox, redactor }
    }
}

impl Tool for WorkspaceBashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Execute a bash command in the workspace. Relative paths resolve in the \
         same workspace used by fs_read/fs_write. Pass `named` to reuse a shell \
         and preserve state such as cd and exported variables. If a command \
         times out it keeps running and returns a pid; call bash again with \
         that pid and action=poll or action=cancel."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command":    { "type": "string",  "description": "Shell command to execute" },
                "named":      { "type": "string",  "description": "Optional named shell session. Calls with the same name preserve shell state." },
                "timeout_ms": { "type": "integer", "description": "Optional timeout in milliseconds" },
                "pid":        { "type": "string",  "description": "Existing bash task pid returned after a timeout" },
                "action":     { "type": "string",  "enum": ["poll", "cancel"], "description": "Action for an existing pid. Defaults to poll when pid is provided." }
            }
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let sandbox = Arc::clone(&self.sandbox);
        let redactor = Arc::clone(&self.redactor);
        async move {
            let pid = arguments["pid"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
            let action = arguments["action"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("poll")
                .to_ascii_lowercase();
            if let Some(pid) = pid {
                return Ok(ToolResult::Output(
                    stream! {
                        let result = match action.as_str() {
                            "poll" => sandbox.bash_poll(&pid).await,
                            "cancel" => sandbox.bash_cancel(&pid).await,
                            other => Err(anyhow::anyhow!("unsupported bash action `{other}`")),
                        };
                        match result {
                            Ok(output) => {
                                let value = bash_task_json(output, &redactor);
                                yield ToolOutput::text(json_text(value));
                            }
                            Err(e) => yield ToolOutput::text(format!("error: {e:#}")),
                        }
                    }
                    .boxed(),
                ));
            }
            let command = arguments["command"]
                .as_str()
                .ok_or_else(|| AgentError::tool("bash", "missing 'command' or 'pid'"))?
                .to_string();
            let named = arguments["named"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
            let timeout_ms = arguments["timeout_ms"].as_u64().unwrap_or(30_000);
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::Delta(format!("$ {}", command));
                let started = Instant::now();
                let cmd_preview = log_preview(&command, 160);
                tracing::info!(
                    command = %cmd_preview,
                    command_len = command.len(),
                    named_session = named.as_deref().unwrap_or(""),
                    timeout_ms,
                    sandbox_kind = %sandbox.kind(),
                    "bash.start"
                );
                match sandbox.bash(&command, named.as_deref(), timeout_ms).await {
                    Ok(output) if output.timed_out || output.status == SandboxBashStatus::Running => {
                        tracing::warn!(
                            command = %cmd_preview,
                            command_len = command.len(),
                            named_session = named.as_deref().unwrap_or(""),
                            timeout_ms,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "bash.failed"
                        );
                        let value = bash_task_json(output, &redactor);
                        yield ToolOutput::text(json_text(value));
                    }
                    Err(e) => {
                        tracing::warn!(
                            command = %cmd_preview,
                            command_len = command.len(),
                            named_session = named.as_deref().unwrap_or(""),
                            timeout_ms,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "bash.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                    Ok(output) => {
                        let structured = !matches!(output.status, SandboxBashStatus::Completed)
                            || output.pid.is_some();
                        let stdout = output.stdout;
                        let stderr = output.stderr;
                        let code   = output.exit_code;
                        let stdout_bytes = stdout.len();
                        let stderr_bytes = stderr.len();
                        if code == 0 {
                            tracing::info!(
                                command = %cmd_preview,
                                command_len = command.len(),
                                named_session = named.as_deref().unwrap_or(""),
                                timeout_ms,
                                sandbox_kind = %sandbox.kind(),
                                exit_code = code,
                                stdout_bytes,
                                stderr_bytes,
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "bash.completed"
                            );
                        } else {
                            tracing::warn!(
                                command = %cmd_preview,
                                command_len = command.len(),
                                named_session = named.as_deref().unwrap_or(""),
                                timeout_ms,
                                sandbox_kind = %sandbox.kind(),
                                exit_code = code,
                                stdout_bytes,
                                stderr_bytes,
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "bash.failed"
                            );
                        }
                        if !structured {
                            let r = format_bash_text(stdout, stderr, code, &redactor);
                            yield ToolOutput::text(r);
                        } else {
                            let value = bash_task_json(SandboxBashOutput {
                                stdout,
                                stderr,
                                exit_code: code,
                                timed_out: output.timed_out,
                                status: output.status,
                                pid: output.pid,
                                os_pid: output.os_pid,
                                message: output.message,
                            }, &redactor);
                            yield ToolOutput::text(json_text(value));
                        }
                    }
                }
            }.boxed()))
        }
    }
}

fn format_bash_text(
    stdout: String,
    stderr: String,
    code: i32,
    redactor: &SharedRedactor,
) -> String {
    let mut r = stdout;
    if !stderr.is_empty() {
        if !r.is_empty() {
            r.push('\n');
        }
        r.push_str("[stderr] ");
        r.push_str(&stderr);
    }
    if code != 0 {
        r.push_str(&format!("\n[exit {code}]"));
    }
    redactor.read().unwrap().redact(&r)
}

fn bash_task_json(output: SandboxBashOutput, redactor: &SharedRedactor) -> serde_json::Value {
    command_task_json(output, redactor, "bash")
}

fn command_task_json(
    output: SandboxBashOutput,
    redactor: &SharedRedactor,
    tool_name: &str,
) -> serde_json::Value {
    let status = match output.status {
        SandboxBashStatus::Completed => "completed",
        SandboxBashStatus::Running => "running",
        SandboxBashStatus::Cancelled => "cancelled",
        SandboxBashStatus::NotFound => "not_found",
    };
    let stdout = redactor.read().unwrap().redact(&output.stdout);
    let stderr = redactor.read().unwrap().redact(&output.stderr);
    let mut value = serde_json::json!({
        "status": status,
        "pid": output.pid,
        "os_pid": output.os_pid,
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": if matches!(output.status, SandboxBashStatus::Completed) { Some(output.exit_code) } else { None },
        "timed_out": output.timed_out,
        "message": output.message,
    });
    if matches!(output.status, SandboxBashStatus::Running) {
        let pid = value["pid"].as_str().unwrap_or_default();
        value["poll_hint"] = serde_json::Value::String(format!(
            "Call {tool_name} again with pid={pid} and action=poll. Use action=cancel to terminate it."
        ));
    }
    value
}

fn json_text(value: serde_json::Value) -> String {
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

// ── WorkspaceSshTool ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct WorkspaceSshTool {
    runner: Arc<SshRunner>,
    redactor: SharedRedactor,
}

impl WorkspaceSshTool {
    pub fn new(redactor: SharedRedactor) -> Self {
        Self {
            runner: Arc::new(SshRunner::new()),
            redactor,
        }
    }
}

impl Tool for WorkspaceSshTool {
    fn name(&self) -> &str {
        "ssh"
    }

    fn description(&self) -> &str {
        "Execute a command on a remote host via the host OpenSSH client. Uses the host ~/.ssh/config, keys, and ssh-agent in batch mode; password and inline private-key parameters are not supported. Pass `named` to reuse a remote shell session for the same host/user/port and preserve state such as cd and exported variables. If a command times out it keeps running and returns a pid; call ssh again with that pid and action=poll or action=cancel."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "host":       { "type": "string",  "description": "OpenSSH host alias or hostname" },
                "user":       { "type": "string",  "description": "Optional SSH username; otherwise OpenSSH config/defaults apply" },
                "port":       { "type": "integer", "description": "Optional SSH port, 1 through 65535" },
                "command":    { "type": "string",  "description": "Remote shell command to execute" },
                "named":      { "type": "string",  "description": "Optional named remote shell session. Calls with the same name preserve state for the same host/user/port." },
                "timeout_ms": { "type": "integer", "description": "Optional timeout in milliseconds" },
                "pid":        { "type": "string",  "description": "Existing ssh task pid returned after a timeout" },
                "action":     { "type": "string",  "enum": ["poll", "cancel"], "description": "Action for an existing pid. Defaults to poll when pid is provided." }
            }
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let runner = Arc::clone(&self.runner);
        let redactor = Arc::clone(&self.redactor);
        async move {
            let pid = arguments["pid"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
            let action = arguments["action"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("poll")
                .to_ascii_lowercase();
            if let Some(pid) = pid {
                return Ok(ToolResult::Output(
                    stream! {
                        let result = match action.as_str() {
                            "poll" => runner.poll(&pid).await,
                            "cancel" => runner.cancel(&pid).await,
                            other => Err(anyhow::anyhow!("unsupported ssh action `{other}`")),
                        };
                        match result {
                            Ok(output) => {
                                let value = command_task_json(output, &redactor, "ssh");
                                yield ToolOutput::text(json_text(value));
                            }
                            Err(e) => yield ToolOutput::text(format!("error: {e:#}")),
                        }
                    }
                    .boxed(),
                ));
            }

            let target =
                parse_ssh_target(&arguments).map_err(|err| AgentError::tool("ssh", err))?;
            let command = arguments["command"]
                .as_str()
                .ok_or_else(|| AgentError::tool("ssh", "missing 'command' or 'pid'"))?
                .to_string();
            let named = arguments["named"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
            if let Some(named) = named.as_deref() {
                validate_ssh_named(named).map_err(|err| AgentError::tool("ssh", err))?;
            }
            let timeout_ms = arguments["timeout_ms"].as_u64().unwrap_or(30_000);
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::Delta(format!("ssh {} $ {}", target.display(), command));
                let started = Instant::now();
                let cmd_preview = log_preview(&command, 160);
                tracing::info!(
                    host = %target.host,
                    user = target.user.as_deref().unwrap_or(""),
                    port = target.port.unwrap_or_default(),
                    command = %cmd_preview,
                    command_len = command.len(),
                    named_session = named.as_deref().unwrap_or(""),
                    timeout_ms,
                    "ssh.start"
                );
                match runner.run(target.clone(), &command, named.as_deref(), timeout_ms).await {
                    Ok(output) if output.timed_out || output.status == SandboxBashStatus::Running => {
                        tracing::warn!(
                            host = %target.host,
                            user = target.user.as_deref().unwrap_or(""),
                            port = target.port.unwrap_or_default(),
                            command = %cmd_preview,
                            command_len = command.len(),
                            named_session = named.as_deref().unwrap_or(""),
                            timeout_ms,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "ssh.timed_out"
                        );
                        let value = command_task_json(output, &redactor, "ssh");
                        yield ToolOutput::text(json_text(value));
                    }
                    Err(e) => {
                        tracing::warn!(
                            host = %target.host,
                            user = target.user.as_deref().unwrap_or(""),
                            port = target.port.unwrap_or_default(),
                            command = %cmd_preview,
                            command_len = command.len(),
                            named_session = named.as_deref().unwrap_or(""),
                            timeout_ms,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "ssh.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                    Ok(output) => {
                        let structured = !matches!(output.status, SandboxBashStatus::Completed)
                            || output.pid.is_some();
                        let stdout = output.stdout;
                        let stderr = output.stderr;
                        let code = output.exit_code;
                        let stdout_bytes = stdout.len();
                        let stderr_bytes = stderr.len();
                        if code == 0 {
                            tracing::info!(
                                host = %target.host,
                                user = target.user.as_deref().unwrap_or(""),
                                port = target.port.unwrap_or_default(),
                                command = %cmd_preview,
                                command_len = command.len(),
                                named_session = named.as_deref().unwrap_or(""),
                                timeout_ms,
                                exit_code = code,
                                stdout_bytes,
                                stderr_bytes,
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "ssh.completed"
                            );
                        } else {
                            tracing::warn!(
                                host = %target.host,
                                user = target.user.as_deref().unwrap_or(""),
                                port = target.port.unwrap_or_default(),
                                command = %cmd_preview,
                                command_len = command.len(),
                                named_session = named.as_deref().unwrap_or(""),
                                timeout_ms,
                                exit_code = code,
                                stdout_bytes,
                                stderr_bytes,
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "ssh.failed"
                            );
                        }
                        if !structured {
                            let r = format_bash_text(stdout, stderr, code, &redactor);
                            yield ToolOutput::text(r);
                        } else {
                            let value = command_task_json(SandboxBashOutput {
                                stdout,
                                stderr,
                                exit_code: code,
                                timed_out: output.timed_out,
                                status: output.status,
                                pid: output.pid,
                                os_pid: output.os_pid,
                                message: output.message,
                            }, &redactor, "ssh");
                            yield ToolOutput::text(json_text(value));
                        }
                    }
                }
            }.boxed()))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SshTarget {
    host: String,
    user: Option<String>,
    port: Option<u16>,
}

impl SshTarget {
    fn display(&self) -> String {
        let user = self
            .user
            .as_ref()
            .map(|user| format!("{user}@"))
            .unwrap_or_default();
        let port = self.port.map(|port| format!(":{port}")).unwrap_or_default();
        format!("{user}{}{port}", self.host)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SshSessionKey {
    named: String,
    target: SshTarget,
}

#[derive(Clone)]
struct SshRunner {
    sessions: Arc<Mutex<HashMap<SshSessionKey, Arc<Mutex<SshSession>>>>>,
    tasks: SshTaskRegistry,
}

impl SshRunner {
    fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            tasks: SshTaskRegistry::new(),
        }
    }

    async fn run(
        &self,
        target: SshTarget,
        command: &str,
        named: Option<&str>,
        timeout_ms: u64,
    ) -> anyhow::Result<SandboxBashOutput> {
        ensure_ssh_available().await?;
        match named.map(str::trim).filter(|value| !value.is_empty()) {
            Some(named) => self.run_named(target, named, command, timeout_ms).await,
            None => self.run_once(target, command, timeout_ms).await,
        }
    }

    async fn run_once(
        &self,
        target: SshTarget,
        command: &str,
        timeout_ms: u64,
    ) -> anyhow::Result<SandboxBashOutput> {
        let mut cmd = TokioCommand::new("ssh");
        cmd.args(ssh_command_args(&target, Some(command)));
        run_ssh_command_with_timeout(cmd, timeout_ms, self.tasks.clone()).await
    }

    async fn run_named(
        &self,
        target: SshTarget,
        named: &str,
        command: &str,
        timeout_ms: u64,
    ) -> anyhow::Result<SandboxBashOutput> {
        let named = validate_ssh_named(named).map_err(anyhow::Error::msg)?;
        if let Some(pid) = self.tasks.active_named_pid(&named).await {
            return Err(anyhow::anyhow!(
                "named ssh session `{named}` is still running task `{pid}`; poll or cancel that pid, wait for it to complete, or use a different named session"
            ));
        }
        let key = SshSessionKey {
            named: named.clone(),
            target,
        };
        let session = {
            let mut sessions = self.sessions.lock().await;
            let conflicts = sessions
                .keys()
                .any(|existing| existing.named == named && existing != &key);
            if conflicts {
                return Err(anyhow::anyhow!(
                    "named ssh session `{named}` is already bound to another host/user/port"
                ));
            }
            if let Some(session) = sessions.get(&key) {
                Arc::clone(session)
            } else {
                let session = Arc::new(Mutex::new(SshSession::start(&key.target).await?));
                sessions.insert(key.clone(), Arc::clone(&session));
                session
            }
        };

        let task = self
            .tasks
            .insert(SshTaskState {
                pid: new_ssh_pid(),
                named: Some(named.clone()),
                stdout: String::new(),
                stderr: String::new(),
                stdout_cursor: 0,
                stderr_cursor: 0,
                os_pid: None,
                exit_code: None,
                status: SshTaskStatus::Running,
                message: None,
                finished_at: None,
            })
            .await;
        let sessions = Arc::clone(&self.sessions);
        let command = command.to_string();
        tokio::spawn({
            let task = Arc::clone(&task);
            async move {
                let result = {
                    let mut session = session.lock().await;
                    {
                        let mut task = task.lock().await;
                        task.os_pid = session.os_pid();
                    }
                    session.run_into_task(&command, Arc::clone(&task)).await
                };
                let mut remove_session = false;
                {
                    let mut task = task.lock().await;
                    match task.status {
                        SshTaskStatus::Cancelled => {
                            remove_session = true;
                        }
                        SshTaskStatus::Running => match result {
                            Ok(exit_code) => {
                                task.exit_code = Some(exit_code);
                                task.status = SshTaskStatus::Completed;
                                task.finished_at = Some(Instant::now());
                            }
                            Err(err) => {
                                task.exit_code = Some(-1);
                                task.status = SshTaskStatus::Completed;
                                task.message = Some(err.to_string());
                                task.finished_at = Some(Instant::now());
                                remove_session = true;
                            }
                        },
                        SshTaskStatus::Completed => {}
                    }
                }
                if remove_session {
                    sessions.lock().await.remove(&key);
                }
            }
        });
        wait_for_ssh_task(task, timeout_ms).await
    }

    async fn poll(&self, pid: &str) -> anyhow::Result<SandboxBashOutput> {
        Ok(self.tasks.poll(pid).await)
    }

    async fn cancel(&self, pid: &str) -> anyhow::Result<SandboxBashOutput> {
        Ok(self.tasks.cancel(pid).await)
    }
}

#[derive(Debug)]
struct SshSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl SshSession {
    async fn start(target: &SshTarget) -> anyhow::Result<Self> {
        let mut cmd = TokioCommand::new("ssh");
        cmd.args(ssh_command_args(target, None));
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let child = cmd.spawn().context("starting named ssh session")?;
        let mut session = Self::from_child(child, "named ssh session")?;
        session.discard_startup_output().await?;
        Ok(session)
    }

    fn from_child(mut child: Child, label: &str) -> anyhow::Result<Self> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("{label} stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("{label} stdout unavailable"))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    fn os_pid(&self) -> Option<u32> {
        self.child.id()
    }

    async fn run_into_task(
        &mut self,
        command: &str,
        task: Arc<Mutex<SshTaskState>>,
    ) -> anyhow::Result<i32> {
        let marker = format!("__REMI_SSH_DONE_{}__", uuid::Uuid::new_v4().simple());
        let wrapped = format!(
            "{command} 2>&1\n__remi_status=$?\nprintf '\\n{marker}:%s\\n' \"$__remi_status\"\n"
        );
        self.stdin
            .write_all(wrapped.as_bytes())
            .await
            .context("writing to named ssh session")?;
        self.stdin
            .flush()
            .await
            .context("flushing named ssh session")?;

        let mut line = String::new();
        loop {
            line.clear();
            let n = self
                .stdout
                .read_line(&mut line)
                .await
                .context("reading named ssh session")?;
            if n == 0 {
                return Err(anyhow::anyhow!("named ssh session exited"));
            }
            if let Some(exit_code) = parse_ssh_marker_line(&line, &marker) {
                trim_ssh_task_stdout_newline(&task).await;
                return Ok(exit_code);
            }
            append_ssh_task_stdout(&task, &line).await;
        }
    }

    async fn discard_startup_output(&mut self) -> anyhow::Result<()> {
        let marker = format!("__REMI_SSH_READY_{}__", uuid::Uuid::new_v4().simple());
        let probe = format!("printf '\\n{marker}\\n'\n");
        self.stdin
            .write_all(probe.as_bytes())
            .await
            .context("writing named ssh startup probe")?;
        self.stdin
            .flush()
            .await
            .context("flushing named ssh startup probe")?;

        let mut line = String::new();
        loop {
            line.clear();
            let n = self
                .stdout
                .read_line(&mut line)
                .await
                .context("reading named ssh startup probe")?;
            if n == 0 {
                return Err(anyhow::anyhow!("named ssh session exited during startup"));
            }
            if line.trim_end() == marker {
                return Ok(());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SshTaskStatus {
    Running,
    Completed,
    Cancelled,
}

#[derive(Debug)]
struct SshTaskState {
    pid: String,
    named: Option<String>,
    stdout: String,
    stderr: String,
    stdout_cursor: usize,
    stderr_cursor: usize,
    os_pid: Option<u32>,
    exit_code: Option<i32>,
    status: SshTaskStatus,
    message: Option<String>,
    finished_at: Option<Instant>,
}

#[derive(Clone, Debug)]
struct SshTaskRegistry {
    tasks: Arc<Mutex<HashMap<String, Arc<Mutex<SshTaskState>>>>>,
}

impl SshTaskRegistry {
    fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn insert(&self, state: SshTaskState) -> Arc<Mutex<SshTaskState>> {
        self.prune().await;
        let pid = state.pid.clone();
        let task = Arc::new(Mutex::new(state));
        self.tasks.lock().await.insert(pid, Arc::clone(&task));
        task
    }

    async fn active_named_pid(&self, named: &str) -> Option<String> {
        self.prune().await;
        let tasks: Vec<_> = self.tasks.lock().await.values().cloned().collect();
        for task in tasks {
            let task = task.lock().await;
            if task.status == SshTaskStatus::Running && task.named.as_deref() == Some(named) {
                return Some(task.pid.clone());
            }
        }
        None
    }

    async fn poll(&self, pid: &str) -> SandboxBashOutput {
        self.prune().await;
        let task = self.tasks.lock().await.get(pid).cloned();
        let Some(task) = task else {
            return ssh_not_found(pid);
        };
        let mut task = task.lock().await;
        match task.status {
            SshTaskStatus::Running => ssh_output_running(&mut task, false),
            SshTaskStatus::Completed => {
                ssh_output_terminal(&mut task, SandboxBashStatus::Completed)
            }
            SshTaskStatus::Cancelled => {
                ssh_output_terminal(&mut task, SandboxBashStatus::Cancelled)
            }
        }
    }

    async fn cancel(&self, pid: &str) -> SandboxBashOutput {
        self.prune().await;
        let task = self.tasks.lock().await.get(pid).cloned();
        let Some(task) = task else {
            return ssh_not_found(pid);
        };
        let os_pid = {
            let mut task = task.lock().await;
            if task.status == SshTaskStatus::Running {
                task.status = SshTaskStatus::Cancelled;
                task.exit_code = Some(-1);
                task.message = Some("ssh task cancelled".to_string());
                task.finished_at = Some(Instant::now());
            }
            task.os_pid
        };
        if let Some(os_pid) = os_pid {
            terminate_ssh_process(os_pid).await;
        }
        let mut task = task.lock().await;
        ssh_output_terminal(&mut task, SandboxBashStatus::Cancelled)
    }

    async fn prune(&self) {
        let tasks: Vec<_> = self
            .tasks
            .lock()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut expired = Vec::new();
        for (pid, task) in tasks {
            let task = task.lock().await;
            if task
                .finished_at
                .map(|finished_at| finished_at.elapsed() >= Duration::from_secs(30 * 60))
                .unwrap_or(false)
            {
                expired.push(pid);
            }
        }
        if !expired.is_empty() {
            let mut tasks = self.tasks.lock().await;
            for pid in expired {
                tasks.remove(&pid);
            }
        }
    }
}

async fn run_ssh_command_with_timeout(
    mut cmd: TokioCommand,
    timeout_ms: u64,
    tasks: SshTaskRegistry,
) -> anyhow::Result<SandboxBashOutput> {
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("running ssh command")?;
    let os_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("ssh command stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("ssh command stderr unavailable"))?;
    let task = tasks
        .insert(SshTaskState {
            pid: new_ssh_pid(),
            named: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_cursor: 0,
            stderr_cursor: 0,
            os_pid,
            exit_code: None,
            status: SshTaskStatus::Running,
            message: None,
            finished_at: None,
        })
        .await;
    spawn_ssh_reader(stdout, Arc::clone(&task), true);
    spawn_ssh_reader(stderr, Arc::clone(&task), false);
    tokio::spawn({
        let wait_task = Arc::clone(&task);
        async move {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let mut task = wait_task.lock().await;
                        if task.status == SshTaskStatus::Running {
                            task.exit_code = Some(status.code().unwrap_or(-1));
                            task.status = SshTaskStatus::Completed;
                            task.finished_at = Some(Instant::now());
                        }
                        break;
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Err(err) => {
                        let mut task = wait_task.lock().await;
                        if task.status == SshTaskStatus::Running {
                            task.exit_code = Some(-1);
                            task.status = SshTaskStatus::Completed;
                            task.message = Some(format!("ssh wait failed: {err}"));
                            task.finished_at = Some(Instant::now());
                        }
                        break;
                    }
                }
            }
            let _ = child.wait().await;
        }
    });
    wait_for_ssh_task(task, timeout_ms).await
}

async fn wait_for_ssh_task(
    task: Arc<Mutex<SshTaskState>>,
    timeout_ms: u64,
) -> anyhow::Result<SandboxBashOutput> {
    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);
    loop {
        {
            let mut task = task.lock().await;
            match task.status {
                SshTaskStatus::Running => {}
                SshTaskStatus::Completed => {
                    let mut output = ssh_output_terminal(&mut task, SandboxBashStatus::Completed);
                    output.pid = None;
                    output.os_pid = None;
                    return Ok(output);
                }
                SshTaskStatus::Cancelled => {
                    return Ok(ssh_output_terminal(&mut task, SandboxBashStatus::Cancelled));
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            _ = &mut timeout => {
                let mut task = task.lock().await;
                return Ok(ssh_output_running(&mut task, true));
            }
        }
    }
}

fn ssh_output_running(task: &mut SshTaskState, timed_out: bool) -> SandboxBashOutput {
    let stdout = take_ssh_incremental(&task.stdout, &mut task.stdout_cursor);
    let stderr = take_ssh_incremental(&task.stderr, &mut task.stderr_cursor);
    SandboxBashOutput {
        stdout,
        stderr,
        exit_code: -1,
        timed_out,
        status: SandboxBashStatus::Running,
        pid: Some(task.pid.clone()),
        os_pid: task.os_pid,
        message: None,
    }
}

fn ssh_output_terminal(task: &mut SshTaskState, status: SandboxBashStatus) -> SandboxBashOutput {
    let stdout = take_ssh_incremental(&task.stdout, &mut task.stdout_cursor);
    let stderr = take_ssh_incremental(&task.stderr, &mut task.stderr_cursor);
    SandboxBashOutput {
        stdout,
        stderr,
        exit_code: task.exit_code.unwrap_or(-1),
        timed_out: false,
        status,
        pid: Some(task.pid.clone()),
        os_pid: task.os_pid,
        message: task.message.clone(),
    }
}

fn ssh_not_found(pid: &str) -> SandboxBashOutput {
    SandboxBashOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: -1,
        timed_out: false,
        status: SandboxBashStatus::NotFound,
        pid: Some(pid.to_string()),
        os_pid: None,
        message: Some("ssh task not found; it may have completed and expired".to_string()),
    }
}

fn take_ssh_incremental(buffer: &str, cursor: &mut usize) -> String {
    let cursor_value = (*cursor).min(buffer.len());
    let out = buffer[cursor_value..].to_string();
    *cursor = buffer.len();
    out
}

fn spawn_ssh_reader<R>(reader: R, task: Arc<Mutex<SshTaskState>>, stdout: bool)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buffer[..n]);
                    let mut task = task.lock().await;
                    if stdout {
                        task.stdout.push_str(&text);
                    } else {
                        task.stderr.push_str(&text);
                    }
                }
                Err(err) => {
                    let mut task = task.lock().await;
                    task.message = Some(format!("ssh reader failed: {err}"));
                    break;
                }
            }
        }
    });
}

async fn append_ssh_task_stdout(task: &Arc<Mutex<SshTaskState>>, text: &str) {
    task.lock().await.stdout.push_str(text);
}

async fn trim_ssh_task_stdout_newline(task: &Arc<Mutex<SshTaskState>>) {
    let mut task = task.lock().await;
    if task.stdout.ends_with('\n') {
        task.stdout.pop();
        if task.stdout.ends_with('\r') {
            task.stdout.pop();
        }
    }
}

fn parse_ssh_marker_line(line: &str, marker: &str) -> Option<i32> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let rest = trimmed.strip_prefix(marker)?.strip_prefix(':')?;
    rest.parse::<i32>().ok()
}

fn new_ssh_pid() -> String {
    format!("ssh_{}", uuid::Uuid::new_v4().simple())
}

async fn terminate_ssh_process(pid: u32) {
    #[cfg(unix)]
    {
        let _ = TokioCommand::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .output()
            .await;
    }
    #[cfg(windows)]
    {
        let _ = TokioCommand::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .await;
    }
}

async fn ensure_ssh_available() -> anyhow::Result<()> {
    match TokioCommand::new("ssh").arg("-V").output().await {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err(anyhow::anyhow!("ssh executable not found in host PATH"))
        }
        Err(err) => Err(err).context("checking ssh executable"),
    }
}

fn parse_ssh_target(arguments: &serde_json::Value) -> Result<SshTarget, String> {
    let host = arguments["host"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing 'host'".to_string())?;
    validate_ssh_token("host", host)?;
    let user = arguments["user"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            validate_ssh_token("user", value)?;
            Ok::<_, String>(value.to_string())
        })
        .transpose()?;
    let port = match arguments.get("port") {
        Some(value) if !value.is_null() => {
            let port = value
                .as_u64()
                .ok_or_else(|| "'port' must be an integer from 1 through 65535".to_string())?;
            if port == 0 || port > u16::MAX as u64 {
                return Err("'port' must be from 1 through 65535".to_string());
            }
            Some(port as u16)
        }
        _ => None,
    };
    Ok(SshTarget {
        host: host.to_string(),
        user,
        port,
    })
}

fn validate_ssh_token(label: &str, value: &str) -> Result<(), String> {
    if value.chars().any(char::is_whitespace) {
        return Err(format!("'{label}' may not contain whitespace"));
    }
    if value.starts_with('-') {
        return Err(format!("'{label}' may not start with '-'"));
    }
    Ok(())
}

fn validate_ssh_named(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("named ssh session may not be empty".to_string());
    }
    if trimmed.len() > 64 {
        return Err("named ssh session must be at most 64 characters".to_string());
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(
            "named ssh session may only contain ASCII letters, numbers, '-' and '_'".to_string(),
        );
    }
    Ok(trimmed.to_string())
}

fn ssh_command_args(target: &SshTarget, command: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-T".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ];
    if let Some(user) = target.user.as_deref() {
        args.push("-l".to_string());
        args.push(user.to_string());
    }
    if let Some(port) = target.port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    args.push(target.host.clone());
    if let Some(command) = command {
        args.push(command.to_string());
    }
    args
}

// ── ManageYourselfTool ────────────────────────────────────────────────────────

pub struct ManageYourselfTool;

impl Tool for ManageYourselfTool {
    fn name(&self) -> &str {
        "manage_yourself"
    }

    fn description(&self) -> &str {
        "Run a remi-cat CLI command against the current host binary for Remi self-management. Only pass a top-level `command` string, for example: {\"command\":\"profile list\"}. Use {\"command\":\"tools --json\"} to inspect every registered tool and configuration diagnostics, including tools outside the active allowlist. Use help commands such as {\"command\":\"help\"} or {\"command\":\"profile agent --help\"} to inspect available CLI commands. The command is parsed as shell-like arguments but is not executed through a shell."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "remi-cat arguments without the binary name, for example: profile list; use help or <command> --help to inspect available commands"
                }
            },
            "required": ["command"]
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        async move {
            let command = arguments
                .get("command")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    AgentError::tool(
                        "manage_yourself",
                        "missing 'command'; use exactly {\"command\":\"profile list\"}",
                    )
                })?
                .to_string();
            let args = parse_manage_yourself_command(&command)?;
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::Delta(format!("remi-cat {}", args.join(" ")));
                tracing::info!(
                    command = %log_preview(&command, 160),
                    command_len = command.len(),
                    argc = args.len(),
                    "manage_yourself.start"
                );
                match run_manage_yourself_command(&args).await {
                    Ok(output) => yield ToolOutput::text(output),
                    Err(error) => {
                        tracing::warn!(
                            command = %log_preview(&command, 160),
                            command_len = command.len(),
                            argc = args.len(),
                            error = %error,
                            "manage_yourself.failed"
                        );
                        yield ToolOutput::text(format!("error: {error:#}"));
                    }
                }
            }))
        }
    }
}

fn parse_manage_yourself_command(command: &str) -> Result<Vec<String>, AgentError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(AgentError::tool(
            "manage_yourself",
            "command must not be empty",
        ));
    }
    let args = shlex::split(trimmed).ok_or_else(|| {
        AgentError::tool(
            "manage_yourself",
            "failed to parse command; check shell quoting",
        )
    })?;
    if args.is_empty() {
        return Err(AgentError::tool(
            "manage_yourself",
            "command must not be empty",
        ));
    }
    Ok(args)
}

async fn run_manage_yourself_command(args: &[String]) -> anyhow::Result<String> {
    let exe = std::env::current_exe().context("resolving current remi-cat executable")?;
    let started = Instant::now();
    tracing::debug!(
        exe = %exe.display(),
        argc = args.len(),
        "manage_yourself.process.start"
    );
    let output = tokio::task::spawn_blocking({
        let args = args.to_vec();
        move || Command::new(&exe).args(args).output()
    })
    .await
    .context("joining manage_yourself command task")?
    .context("running remi-cat command")?;
    let stdout_bytes = output.stdout.len();
    let stderr_bytes = output.stderr.len();
    let exit_code = output.status.code();
    if output.status.success() {
        tracing::info!(
            exit_code = exit_code.unwrap_or(-1),
            stdout_bytes,
            stderr_bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "manage_yourself.completed"
        );
    } else {
        tracing::warn!(
            exit_code = exit_code.unwrap_or(-1),
            stdout_bytes,
            stderr_bytes,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "manage_yourself.failed"
        );
    }
    Ok(format_command_output(output))
}

fn format_command_output(output: std::process::Output) -> String {
    let mut text = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.is_empty() {
        text.push_str(stdout.trim_end());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr] ");
        text.push_str(stderr.trim_end());
    }
    if let Some(code) = output.status.code() {
        if code != 0 {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("[exit {code}]"));
        }
    } else {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[terminated by signal]");
    }
    text
}

// ── RootedFsReadTool ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FsReadLastRecord {
    path: String,
    start: usize,
    end: usize,
    requested_length: usize,
    total_bytes: usize,
    metadata_len: u64,
    modified_ms: Option<u64>,
}

fn fs_read_duplicate_warning(
    user_state: &Arc<RwLock<Value>>,
    record: FsReadLastRecord,
) -> Option<String> {
    let mut state = user_state.write().unwrap();
    if !state.is_object() {
        *state = serde_json::json!({});
    }

    let previous = state
        .get(FS_READ_LAST_STATE_KEY)
        .and_then(|value| serde_json::from_value::<FsReadLastRecord>(value.clone()).ok());

    let warning = previous.and_then(|previous| {
        let same_file_version = previous.path == record.path
            && previous.total_bytes == record.total_bytes
            && previous.metadata_len == record.metadata_len
            && previous.modified_ms == record.modified_ms;
        let same_range = previous.start == record.start && previous.end == record.end;
        if !same_file_version || !same_range {
            return None;
        }

        let mut warning = format!(
            "warning: this fs_read request repeats a range already read in this session, and the file appears unchanged (path={}, offset={}, length={}).",
            record.path,
            record.start,
            record.end.saturating_sub(record.start)
        );
        if record.end < record.total_bytes {
            warning.push_str(&format!(" Continue with offset={} to read the next unread chunk.", record.end));
        }
        Some(warning)
    });

    if let Some(object) = state.as_object_mut() {
        if let Ok(value) = serde_json::to_value(record) {
            object.insert(FS_READ_LAST_STATE_KEY.to_string(), value);
        }
    }

    warning
}

pub struct RootedFsReadTool {
    pub sandbox: Arc<dyn Sandbox>,
    pub redactor: SharedRedactor,
}

impl Tool for RootedFsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Read a file in the workspace. Text files support `offset` + `length` for chunked reading. \
         Common images (JPEG, PNG, GIF, WebP) are auto-detected and returned inline as images; \
         for those files `offset` and `length` are ignored. Always check `[total_bytes]` in text results \
         and call again with offset += length if needed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string",  "description": "File path (relative to workspace root)" },
                "offset": { "type": "integer", "description": "Byte offset to start for text files (default 0)" },
                "length": {
                    "type": "integer",
                    "description": format!("Max bytes to return for text files (default {DEFAULT_FS_READ_LENGTH})")
                }
            },
            "required": ["path"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let sandbox = Arc::clone(&self.sandbox);
        let redactor = Arc::clone(&self.redactor);
        let user_state = Arc::clone(&ctx.user_state);
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_read", "missing 'path'"))?
                .to_string();
            let offset = arguments["offset"].as_u64().unwrap_or(0) as usize;
            let length = arguments["length"]
                .as_u64()
                .unwrap_or(DEFAULT_FS_READ_LENGTH as u64) as usize;
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    offset,
                    length,
                    sandbox_kind = %sandbox.kind(),
                    "fs_read.start"
                );
                match sandbox.read(&path_str).await {
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            offset,
                            length,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_read.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                    Ok(bytes) => {
                        let total = bytes.len();
                        let metadata = match sandbox.metadata(&path_str).await {
                            Ok(metadata) => Some(metadata),
                            Err(e) => {
                                tracing::warn!(
                                    path = %path_str,
                                    sandbox_kind = %sandbox.kind(),
                                    error = %e,
                                    "fs_read.metadata.failed"
                                );
                                None
                            }
                        };
                        if let Some(mime_type) = detect_inline_image_media_type(&path_str, &bytes) {
                            tracing::info!(
                                path = %path_str,
                                total_bytes = total,
                                mime_type,
                                inline_image = true,
                                sandbox_kind = %sandbox.kind(),
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "fs_read.completed"
                            );
                            let summary = redactor
                                .read()
                                .unwrap()
                                .redact(&inline_image_summary(&path_str, mime_type, total));
                            let data_url = format!(
                                "data:{mime_type};base64,{}",
                                base64::engine::general_purpose::STANDARD.encode(&bytes)
                            );
                            yield ToolOutput::Result(Content::parts(vec![
                                ContentPart::text(summary),
                                ContentPart::image_url(data_url),
                            ]));
                            return;
                        }

                        let start = offset.min(total);
                        let end   = (start + length).min(total);
                        let duplicate_warning = metadata.as_ref().and_then(|metadata| {
                            fs_read_duplicate_warning(
                                &user_state,
                                FsReadLastRecord {
                                    path: path_str.clone(),
                                    start,
                                    end,
                                    requested_length: length,
                                    total_bytes: total,
                                    metadata_len: metadata.len,
                                    modified_ms: metadata.modified_ms,
                                },
                            )
                        });
                        let text  = String::from_utf8_lossy(&bytes[start..end]).into_owned();
                        let remaining = total.saturating_sub(end);
                        tracing::info!(
                            path = %path_str,
                            start,
                            returned_bytes = end.saturating_sub(start),
                            total_bytes = total,
                            remaining,
                            inline_image = false,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_read.completed"
                        );
                        let text = redactor.read().unwrap().redact(&text);
                        let mut r = text;
                        if let Some(warning) = duplicate_warning {
                            r = format!("{warning}\n\n{r}");
                        }
                        r.push_str(&format!("\n[offset={start} length={} total_bytes={total} remaining={remaining}]", end - start));
                        if remaining > 0 {
                            r.push_str(&format!("\n[{remaining} bytes remaining — call fs_read again with offset={end}]"));
                        }
                        yield ToolOutput::text(r);
                    }
                }
            }))
        }
    }
}

// ── RootedFsWriteTool ─────────────────────────────────────────────────────────

pub struct RootedFsWriteTool {
    pub sandbox: Arc<dyn Sandbox>,
}

impl Tool for RootedFsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn description(&self) -> &str {
        "Write text to a file in the workspace. Path is relative to workspace root. \
         Parent directories must already exist. Prefer apply_patch for editing existing \
         files; use fs_write only when creating a new file or intentionally replacing an \
         entire file after reading it."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "File path (relative to workspace root)" },
                "content": { "type": "string", "description": "Text content to write" }
            },
            "required": ["path", "content"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let sandbox = Arc::clone(&self.sandbox);
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_write", "missing 'path'"))?
                .to_string();
            let content = arguments["content"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_write", "missing 'content'"))?
                .to_string();
            let bytes = content.len();
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    bytes,
                    sandbox_kind = %sandbox.kind(),
                    "fs_write.start"
                );
                match sandbox.write(&path_str, content.as_bytes()).await {
                    Ok(()) => {
                        tracing::info!(
                            path = %path_str,
                            bytes,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_write.completed"
                        );
                        yield ToolOutput::text(format!("wrote {bytes} bytes to {path_str}"));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            bytes,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_write.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                }
            }))
        }
    }
}

// ── RootedFsApplyPatchTool ────────────────────────────────────────────────────

pub struct RootedFsApplyPatchTool {
    pub sandbox: Arc<dyn Sandbox>,
}

impl Tool for RootedFsApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> &str {
        "Apply a focused multi-file patch in the workspace. Prefer this for edits to \
         existing files. The patch must be a standard unified diff, such as output \
         from `git diff` or `diff -u`, with `---` / `+++` file headers and `@@` \
         hunks. Every context line, including a blank one, starts with a space. \
         `diff --git` headers are accepted. Each old hunk must match exactly once."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Standard unified diff text, such as output from git diff or diff -u. Prefix blank context lines with a space."
                },
                "workdir": {
                    "type": "string",
                    "description": "Optional workspace-relative directory that patch paths are relative to, e.g. `repo` for a diff generated inside repo/"
                }
            },
            "required": ["patch"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let sandbox = Arc::clone(&self.sandbox);
        async move {
            let patch = arguments["patch"]
                .as_str()
                .ok_or_else(|| AgentError::tool("apply_patch", "missing 'patch'"))?
                .to_string();
            let workdir = arguments["workdir"].as_str().map(str::to_string);
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    patch_bytes = patch.len(),
                    workdir = workdir.as_deref().unwrap_or(""),
                    sandbox_kind = %sandbox.kind(),
                    "apply_patch.start"
                );
                match apply_patch_to_sandbox(sandbox.as_ref(), &patch, workdir.as_deref()).await {
                    Ok(summary) => {
                        tracing::info!(
                            patch_bytes = patch.len(),
                            workdir = workdir.as_deref().unwrap_or(""),
                            added = summary.added.len(),
                            updated = summary.updated.len(),
                            deleted = summary.deleted.len(),
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "apply_patch.completed"
                        );
                        yield ToolOutput::text(summary.to_string());
                    }
                    Err(e) => {
                        tracing::warn!(
                            patch_bytes = patch.len(),
                            workdir = workdir.as_deref().unwrap_or(""),
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "apply_patch.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                }
            }))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedPatchOp {
    Add { path: String, content: String },
    Delete { path: String },
    Update { path: String, hunks: Vec<PatchHunk> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PatchHunk {
    old: String,
    new: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ApplyPatchSummary {
    added: Vec<String>,
    updated: Vec<String>,
    deleted: Vec<String>,
}

impl std::fmt::Display for ApplyPatchSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!(
                "added {} file(s): {}",
                self.added.len(),
                self.added.join(", ")
            ));
        }
        if !self.updated.is_empty() {
            parts.push(format!(
                "updated {} file(s): {}",
                self.updated.len(),
                self.updated.join(", ")
            ));
        }
        if !self.deleted.is_empty() {
            parts.push(format!(
                "deleted {} file(s): {}",
                self.deleted.len(),
                self.deleted.join(", ")
            ));
        }
        if parts.is_empty() {
            write!(f, "patch applied with no file changes")
        } else {
            write!(f, "patch applied: {}", parts.join("; "))
        }
    }
}

async fn apply_patch_to_sandbox(
    sandbox: &dyn Sandbox,
    patch: &str,
    workdir: Option<&str>,
) -> anyhow::Result<ApplyPatchSummary> {
    let mut ops = parse_apply_patch(patch).map_err(|err| anyhow::anyhow!(err))?;
    if let Some(raw_workdir) = workdir {
        if let Some(workdir) = normalize_patch_workdir(raw_workdir)? {
            for op in &mut ops {
                prefix_patch_op_path(op, &workdir);
            }
        }
    }
    let mut seen_paths = HashSet::new();
    let mut staged: HashMap<String, Option<String>> = HashMap::new();

    for op in &ops {
        let path = patch_op_path(op);
        if !seen_paths.insert(path.to_string()) {
            anyhow::bail!("patch contains multiple operations for {path}");
        }
        match op {
            ParsedPatchOp::Add { path, content } => {
                if sandbox.read(path).await.is_ok() {
                    anyhow::bail!("cannot add {path}: file already exists");
                }
                staged.insert(path.clone(), Some(content.clone()));
            }
            ParsedPatchOp::Delete { path } => {
                sandbox
                    .read(path)
                    .await
                    .with_context(|| format!("cannot delete {path}"))?;
                staged.insert(path.clone(), None);
            }
            ParsedPatchOp::Update { path, hunks } => {
                let bytes = sandbox
                    .read(path)
                    .await
                    .with_context(|| format!("cannot update {path}"))?;
                let mut content = String::from_utf8(bytes)
                    .with_context(|| format!("cannot update {path}: file is not UTF-8 text"))?;
                for hunk in hunks {
                    if hunk.old.is_empty() {
                        anyhow::bail!("update hunk for {path} has no old content");
                    }
                    let count = content.matches(&hunk.old).count();
                    if count == 0 {
                        anyhow::bail!("update hunk did not match {path}");
                    }
                    if count > 1 {
                        anyhow::bail!(
                            "update hunk matched {count} times in {path}; add more context"
                        );
                    }
                    content = content.replacen(&hunk.old, &hunk.new, 1);
                }
                staged.insert(path.clone(), Some(content));
            }
        }
    }

    let mut summary = ApplyPatchSummary::default();
    for op in ops {
        match op {
            ParsedPatchOp::Add { path, .. } => {
                let content = staged
                    .remove(&path)
                    .and_then(|value| value)
                    .unwrap_or_default();
                sandbox
                    .write(&path, content.as_bytes())
                    .await
                    .with_context(|| format!("adding {path}"))?;
                summary.added.push(path);
            }
            ParsedPatchOp::Delete { path } => {
                sandbox
                    .remove(&path, false)
                    .await
                    .with_context(|| format!("deleting {path}"))?;
                let _ = staged.remove(&path);
                summary.deleted.push(path);
            }
            ParsedPatchOp::Update { path, .. } => {
                let content = staged
                    .remove(&path)
                    .and_then(|value| value)
                    .unwrap_or_default();
                sandbox
                    .write(&path, content.as_bytes())
                    .await
                    .with_context(|| format!("updating {path}"))?;
                summary.updated.push(path);
            }
        }
    }

    Ok(summary)
}

fn patch_op_path(op: &ParsedPatchOp) -> &str {
    match op {
        ParsedPatchOp::Add { path, .. }
        | ParsedPatchOp::Delete { path }
        | ParsedPatchOp::Update { path, .. } => path,
    }
}

fn prefix_patch_op_path(op: &mut ParsedPatchOp, workdir: &str) {
    let path = match op {
        ParsedPatchOp::Add { path, .. }
        | ParsedPatchOp::Delete { path }
        | ParsedPatchOp::Update { path, .. } => path,
    };
    if !path.starts_with(&format!("{workdir}/")) {
        *path = format!("{workdir}/{path}");
    }
}

fn normalize_patch_workdir(workdir: &str) -> anyhow::Result<Option<String>> {
    let workdir = workdir.trim().trim_matches('/');
    if workdir.is_empty() || workdir == "." {
        return Ok(None);
    }
    if workdir
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        anyhow::bail!("invalid apply_patch workdir: {workdir}");
    }
    Ok(Some(workdir.to_string()))
}

fn parse_apply_patch(patch: &str) -> Result<Vec<ParsedPatchOp>, String> {
    parse_unified_patch(patch)
}

fn parse_unified_patch(patch: &str) -> Result<Vec<ParsedPatchOp>, String> {
    let lines: Vec<&str> = patch.split_inclusive('\n').collect();
    if lines.is_empty() {
        return Err("patch must not be empty".to_string());
    }
    let mut index = 0;
    let mut ops = Vec::new();
    while index < lines.len() {
        let line = trim_line(lines[index]);
        if line.is_empty()
            || line.starts_with("diff --git ")
            || is_unified_patch_metadata_line(line)
        {
            index += 1;
            continue;
        }

        let Some(old_path_raw) = line.strip_prefix("--- ") else {
            return Err(format!("expected unified diff file header, got: {line}"));
        };
        index += 1;
        if index >= lines.len() {
            return Err("missing +++ file header".to_string());
        }
        let new_header = trim_line(lines[index]);
        let Some(new_path_raw) = new_header.strip_prefix("+++ ") else {
            return Err(format!("expected +++ file header, got: {new_header}"));
        };
        index += 1;

        let old_path = parse_unified_path(old_path_raw)?;
        let new_path = parse_unified_path(new_path_raw)?;
        let path = match (old_path.as_deref(), new_path.as_deref()) {
            (None, None) => return Err("patch cannot use /dev/null for both paths".to_string()),
            (None, Some(path)) | (Some(path), None) | (Some(_), Some(path)) => path.to_string(),
        };

        let mut hunks = Vec::new();
        while index < lines.len() {
            let marker = trim_line(lines[index]);
            if marker.starts_with("diff --git ") || marker.starts_with("--- ") {
                break;
            }
            if is_unified_patch_metadata_line(marker) {
                index += 1;
                continue;
            }
            if !marker.starts_with("@@") {
                return Err(format!(
                    "file {path} expected @@ hunk marker, got: {marker}"
                ));
            }
            index += 1;
            let mut old = String::new();
            let mut new = String::new();
            let mut saw_line = false;
            while index < lines.len() {
                let next = trim_line(lines[index]);
                if next.starts_with("@@")
                    || next.starts_with("diff --git ")
                    || next.starts_with("--- ")
                {
                    break;
                }
                let line = lines[index];
                if let Some(rest) = line.strip_prefix(' ') {
                    old.push_str(rest);
                    new.push_str(rest);
                } else if let Some(rest) = line.strip_prefix('-') {
                    old.push_str(rest);
                } else if let Some(rest) = line.strip_prefix('+') {
                    new.push_str(rest);
                } else if next == r"\ No newline at end of file" {
                    // Informational marker emitted by unified diff.
                } else if next.is_empty()
                    && lines.get(index + 1).is_some_and(|following| {
                        following.starts_with([' ', '-', '+'])
                            || trim_line(following) == r"\ No newline at end of file"
                    })
                {
                    // Be tolerant of a common generated-diff mistake: a blank context
                    // line written as "\n" instead of the required " \n".
                    old.push_str(line);
                    new.push_str(line);
                } else {
                    return Err(format!(
                        "file {path} patch line {} is invalid inside a hunk: {next:?}; \
                         hunk lines must start with space, '-' or '+'",
                        index + 1
                    ));
                }
                saw_line = true;
                index += 1;
            }
            if !saw_line {
                return Err(format!("file {path} has an empty hunk"));
            }
            hunks.push(PatchHunk { old, new });
        }

        if hunks.is_empty() {
            return Err(format!("file {path} must contain at least one hunk"));
        }
        match (old_path, new_path) {
            (None, Some(_)) => {
                let content = hunks.into_iter().map(|hunk| hunk.new).collect();
                ops.push(ParsedPatchOp::Add { path, content });
            }
            (Some(_), None) => ops.push(ParsedPatchOp::Delete { path }),
            (Some(_), Some(_)) => ops.push(ParsedPatchOp::Update { path, hunks }),
            (None, None) => unreachable!("handled above"),
        }
    }

    if ops.is_empty() {
        return Err("patch contains no file changes".to_string());
    }
    Ok(ops)
}

fn parse_unified_path(path: &str) -> Result<Option<String>, String> {
    let path = path
        .split_whitespace()
        .next()
        .ok_or_else(|| "patch file path must not be empty".to_string())?;
    if path == "/dev/null" {
        return Ok(None);
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    if path.is_empty() {
        return Err("patch file path must not be empty".to_string());
    }
    Ok(Some(path.to_string()))
}

fn trim_line(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn is_unified_patch_metadata_line(line: &str) -> bool {
    line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("dissimilarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
}

// ── RootedFsCreateTool ────────────────────────────────────────────────────────

pub struct RootedFsCreateTool {
    pub sandbox: Arc<dyn Sandbox>,
}

impl Tool for RootedFsCreateTool {
    fn name(&self) -> &str {
        "fs_mkdir"
    }
    fn description(&self) -> &str {
        "Create a directory in the workspace. Set recursive=true for mkdir -p behaviour."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": "Directory path (relative to workspace root)" },
                "recursive": { "type": "boolean", "description": "Create parent dirs (default false)" }
            },
            "required": ["path"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let sandbox = Arc::clone(&self.sandbox);
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_mkdir", "missing 'path'"))?
                .to_string();
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    recursive,
                    sandbox_kind = %sandbox.kind(),
                    "fs_mkdir.start"
                );
                match sandbox.mkdir(&path_str, recursive).await {
                    Ok(()) => {
                        tracing::info!(
                            path = %path_str,
                            recursive,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_mkdir.completed"
                        );
                        yield ToolOutput::text(format!("created {path_str}"));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            recursive,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_mkdir.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                }
            }))
        }
    }
}

// ── RootedFsRemoveTool ────────────────────────────────────────────────────────

pub struct RootedFsRemoveTool {
    pub sandbox: Arc<dyn Sandbox>,
}

impl Tool for RootedFsRemoveTool {
    fn name(&self) -> &str {
        "fs_remove"
    }
    fn description(&self) -> &str {
        "Remove a file or directory in the workspace. Set recursive=true to remove a directory tree."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": "Path to remove (relative to workspace root)" },
                "recursive": { "type": "boolean", "description": "Remove directory recursively (default false)" }
            },
            "required": ["path"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let sandbox = Arc::clone(&self.sandbox);
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_remove", "missing 'path'"))?
                .to_string();
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    recursive,
                    sandbox_kind = %sandbox.kind(),
                    "fs_remove.start"
                );
                match sandbox.remove(&path_str, recursive).await {
                    Ok(()) => {
                        tracing::info!(
                            path = %path_str,
                            recursive,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_remove.completed"
                        );
                        yield ToolOutput::text(format!("removed {path_str}"));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            recursive,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_remove.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                }
            }))
        }
    }
}

// ── RootedFsLsTool ────────────────────────────────────────────────────────────

pub struct RootedFsLsTool {
    pub sandbox: Arc<dyn Sandbox>,
    pub redactor: SharedRedactor,
}

impl Tool for RootedFsLsTool {
    fn name(&self) -> &str {
        "fs_ls"
    }
    fn description(&self) -> &str {
        "List the contents of a directory in the workspace."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path (relative to workspace root, default '.')" }
            }
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let sandbox = Arc::clone(&self.sandbox);
        let redactor = Arc::clone(&self.redactor);
        async move {
            let path_str = arguments["path"].as_str().unwrap_or(".").to_string();
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    sandbox_kind = %sandbox.kind(),
                    "fs_ls.start"
                );
                match sandbox.list(&path_str).await {
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_ls.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                    Ok(entries) => {
                        tracing::info!(
                            path = %path_str,
                            entries = entries.len(),
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_ls.completed"
                        );
                        let listing = entries.join("\n");
                        let listing = redactor.read().unwrap().redact(&listing);
                        yield ToolOutput::text(listing);
                    }
                }
            }))
        }
    }
}

// ── ExaSearchTool ─────────────────────────────────────────────────────────────

const EXA_API_URL: &str = "https://api.exa.ai/search";

pub struct ExaSearchTool {
    num_results: usize,
}

impl ExaSearchTool {
    pub fn new() -> Self {
        Self { num_results: 5 }
    }
}

impl Tool for ExaSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web via Exa. Returns titles, URLs, and content highlights. \
         Use for current events, documentation, or any topic needing fresh data. \
         Requires EXA_API_KEY secret to be set."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query":       { "type": "string",  "description": "Search query" },
                "num_results": { "type": "integer", "description": "Max results (default 5)" }
            },
            "required": ["query"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let default_n = self.num_results;
        async move {
            let api_key = match std::env::var("EXA_API_KEY") {
                Ok(k) if !k.is_empty() => k,
                _ => return Err(AgentError::tool(
                    "web_search",
                    "EXA_API_KEY is not set — use `/secrets set EXA_API_KEY <key>` to enable web search",
                )),
            };
            let query = arguments["query"]
                .as_str()
                .ok_or_else(|| AgentError::tool("web_search", "missing 'query'"))?
                .to_string();
            let num = arguments["num_results"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(default_n);

            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    query = %log_preview(&query, 160),
                    query_len = query.len(),
                    num_results = num,
                    "web_search.start"
                );
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new());
                let body = serde_json::json!({
                    "query": query,
                    "numResults": num,
                    "contents": { "text": { "maxCharacters": 1000 } }
                });
                match client
                    .post(EXA_API_URL)
                    .header("x-api-key", &api_key)
                    .json(&body)
                    .send()
                    .await
                {
                    Err(e) => {
                        tracing::warn!(
                            query = %log_preview(&query, 160),
                            query_len = query.len(),
                            num_results = num,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "web_search.failed"
                        );
                        yield ToolOutput::text(format!("search request failed: {e}"));
                    }
                    Ok(resp) => {
                        match resp.json::<serde_json::Value>().await {
                            Err(e) => {
                                tracing::warn!(
                                    query = %log_preview(&query, 160),
                                    query_len = query.len(),
                                    num_results = num,
                                    elapsed_ms = started.elapsed().as_millis() as u64,
                                    error = %e,
                                    "web_search.failed"
                                );
                                yield ToolOutput::text(format!("parse error: {e}"));
                            }
                            Ok(data) => {
                                let mut out = String::new();
                                if let Some(results) = data["results"].as_array() {
                                    tracing::info!(
                                        query = %log_preview(&query, 160),
                                        query_len = query.len(),
                                        num_results = num,
                                        results = results.len(),
                                        elapsed_ms = started.elapsed().as_millis() as u64,
                                        "web_search.completed"
                                    );
                                    for (i, r) in results.iter().enumerate() {
                                        let title = r["title"].as_str().unwrap_or("(no title)");
                                        let url   = r["url"].as_str().unwrap_or("");
                                        let text  = r["text"].as_str().unwrap_or("");
                                        out.push_str(&format!("{}. **{}**\n   {}\n   {}\n\n", i+1, title, url, text.trim()));
                                    }
                                } else {
                                    tracing::info!(
                                        query = %log_preview(&query, 160),
                                        query_len = query.len(),
                                        num_results = num,
                                        results = 0,
                                        elapsed_ms = started.elapsed().as_millis() as u64,
                                        "web_search.completed"
                                    );
                                }
                                if out.is_empty() { out = "No results found.".into(); }
                                yield ToolOutput::text(out.trim().to_string());
                            }
                        }
                    }
                }
            }))
        }
    }
}

// ── NowTool ───────────────────────────────────────────────────────────────────

/// Returns the current date and time with timezone support.
/// Includes Unix timestamp and day of week.
pub struct NowTool;

impl Tool for NowTool {
    fn name(&self) -> &str {
        "now"
    }
    fn description(&self) -> &str {
        "Return the current date and time with timezone, Unix timestamp, and weekday. \
         Use this whenever you need to know what time or date it is."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "Optional timezone offset like '+08:00', '-05:00', '+0800', 'UTC', 'local', or 'Asia/Shanghai'. Defaults to the system local timezone."
                }
            }
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        async move {
            use chrono::{Datelike, FixedOffset, TimeZone, Timelike, Utc, Weekday};

            let timezone_arg = arguments
                .get("timezone")
                .and_then(|v| v.as_str())
                .unwrap_or("local");
            let timezone = parse_timezone_spec(timezone_arg).ok_or_else(|| {
                AgentError::tool(
                    "now",
                    format!(
                        "invalid timezone `{timezone_arg}`; use `local`, `UTC`, an offset like `+08:00`, or `Asia/Shanghai`"
                    ),
                )
            })?;

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let unix_secs = now.as_secs();

            // Use chrono for timezone-aware datetime
            let utc_dt = Utc
                .timestamp_opt(unix_secs as i64, 0)
                .single()
                .unwrap_or_else(|| Utc::now());

            let offset = FixedOffset::east_opt(timezone.offset_seconds)
                .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
            let local_dt = utc_dt.with_timezone(&offset);

            let weekday_en = match local_dt.weekday() {
                Weekday::Mon => "Monday",
                Weekday::Tue => "Tuesday",
                Weekday::Wed => "Wednesday",
                Weekday::Thu => "Thursday",
                Weekday::Fri => "Friday",
                Weekday::Sat => "Saturday",
                Weekday::Sun => "Sunday",
            };
            let weekday_cn = match local_dt.weekday() {
                Weekday::Mon => "周一",
                Weekday::Tue => "周二",
                Weekday::Wed => "周三",
                Weekday::Thu => "周四",
                Weekday::Fri => "周五",
                Weekday::Sat => "周六",
                Weekday::Sun => "周日",
            };

            let formatted = serde_json::json!({
                "datetime": local_dt.to_rfc3339(),
                "date": format!("{:04}-{:02}-{:02}", local_dt.year(), local_dt.month(), local_dt.day()),
                "time": format!("{:02}:{:02}:{:02}", local_dt.hour(), local_dt.minute(), local_dt.second()),
                "timezone": timezone.name,
                "utc_offset": format_utc_offset(timezone.offset_seconds),
                "unix_timestamp": unix_secs,
                "unix_timestamp_ms": now.as_millis(),
                "weekday": {
                    "english": weekday_en,
                    "chinese": weekday_cn,
                    "iso_number": local_dt.weekday().number_from_monday(),
                },
            })
            .to_string();
            Ok(ToolResult::Output(async_stream::stream! {
                yield ToolOutput::text(formatted);
            }))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedTimezone {
    name: String,
    offset_seconds: i32,
}

/// Parse a timezone string into an offset in seconds east of UTC.
/// Supports formats: "+08:00", "+0800", "-05:00", "UTC", "local", and a small
/// set of fixed-offset aliases used by Remi deployments.
fn parse_timezone_spec(s: &str) -> Option<ParsedTimezone> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("local") {
        let now = chrono::Local::now();
        let offset_seconds = now.offset().local_minus_utc();
        return Some(ParsedTimezone {
            name: "local".to_string(),
            offset_seconds,
        });
    }
    if s.eq_ignore_ascii_case("UTC") || s == "Z" || s == "z" {
        return Some(ParsedTimezone {
            name: "UTC".to_string(),
            offset_seconds: 0,
        });
    }
    if s.eq_ignore_ascii_case("Asia/Shanghai") || s.eq_ignore_ascii_case("Asia/Chongqing") {
        return Some(ParsedTimezone {
            name: "Asia/Shanghai".to_string(),
            offset_seconds: 8 * 3600,
        });
    }

    // Try to parse as "+HH:MM" or "+HHMM" or "-HH:MM" or "-HHMM"
    let (sign, rest) = if let Some(rest) = s.strip_prefix('+') {
        (1, rest)
    } else if let Some(rest) = s.strip_prefix('-') {
        (-1, rest)
    } else {
        return None;
    };

    let (hours, minutes): (i32, i32) = if let Some(pos) = rest.find(':') {
        let h = rest[..pos].parse().unwrap_or(0);
        let m = rest[pos + 1..].parse().unwrap_or(0);
        (h, m)
    } else if rest.len() == 4 {
        let h = rest[..2].parse().unwrap_or(0);
        let m = rest[2..].parse().unwrap_or(0);
        (h, m)
    } else {
        return None;
    };

    if hours > 14 || minutes > 59 {
        return None;
    }

    let offset_seconds = sign * (hours * 3600 + minutes * 60);
    Some(ParsedTimezone {
        name: format_utc_offset(offset_seconds),
        offset_seconds,
    })
}

fn format_utc_offset(offset_seconds: i32) -> String {
    let offset_sign = if offset_seconds >= 0 { '+' } else { '-' };
    let offset_abs = offset_seconds.abs();
    let offset_h = offset_abs / 3600;
    let offset_m = (offset_abs % 3600) / 60;
    format!("{offset_sign}{offset_h:02}:{offset_m:02}")
}

/// Pause the tool loop briefly so the agent can wait before polling another tool.
pub struct SleepTool;

impl Tool for SleepTool {
    fn name(&self) -> &str {
        "sleep"
    }

    fn description(&self) -> &str {
        "Sleep for a short duration in seconds. Use this before polling a background task again. Maximum 10 seconds."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "seconds": {
                    "type": "number",
                    "description": "Sleep duration in seconds. Must be between 0 and 10."
                }
            },
            "required": ["seconds"]
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        async move {
            let seconds = arguments["seconds"]
                .as_f64()
                .ok_or_else(|| AgentError::tool("sleep", "missing or invalid 'seconds'"))?;
            if !seconds.is_finite() || !(0.0..=10.0).contains(&seconds) {
                return Err(AgentError::tool(
                    "sleep",
                    "seconds must be a finite number between 0 and 10",
                ));
            }

            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            Ok(ToolResult::Output(async_stream::stream! {
                yield ToolOutput::text(serde_json::json!({
                    "slept_seconds": seconds,
                    "status": "completed"
                }).to_string());
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_command_output, format_utc_offset, parse_apply_patch, parse_manage_yourself_command,
        parse_ssh_target, parse_timezone_spec, ssh_command_args, validate_ssh_named, NowTool,
        ParsedPatchOp, PatchHunk, RootedFsApplyPatchTool, RootedFsReadTool, SecretRedactor,
        SshTarget, WorkspaceBashTool,
    };
    use crate::sandbox::NoSandbox;
    use futures::StreamExt;
    use remi_agentloop::prelude::{
        AgentConfig, Content, Tool, ToolContext, ToolOutput, ToolResult,
    };
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    const ONE_BY_ONE_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("remi-fs-read-test-{}", uuid::Uuid::new_v4()))
    }

    fn test_tool_context() -> ToolContext {
        ToolContext {
            config: AgentConfig::default(),
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

    #[test]
    fn now_timezone_parser_accepts_offsets_and_aliases() {
        assert_eq!(parse_timezone_spec("UTC").unwrap().offset_seconds, 0);
        assert_eq!(
            parse_timezone_spec("Asia/Shanghai").unwrap().offset_seconds,
            8 * 3600
        );
        assert_eq!(
            parse_timezone_spec("+08:00").unwrap().offset_seconds,
            8 * 3600
        );
        assert_eq!(
            parse_timezone_spec("-0530").unwrap().offset_seconds,
            -19_800
        );
        assert_eq!(format_utc_offset(-19_800), "-05:30");
        assert!(parse_timezone_spec("not-a-zone").is_none());
    }

    #[tokio::test]
    async fn now_tool_returns_timezone_timestamp_and_weekday() {
        let content = collect_tool_content(
            <NowTool as Tool>::execute(
                &NowTool,
                json!({ "timezone": "Asia/Shanghai" }),
                None,
                &test_tool_context(),
            )
            .await
            .expect("now should return tool output"),
        )
        .await;
        let value: serde_json::Value =
            serde_json::from_str(&content.text_content()).expect("now output should be json");

        assert_eq!(value["timezone"], "Asia/Shanghai");
        assert_eq!(value["utc_offset"], "+08:00");
        assert!(value["datetime"].as_str().unwrap().contains("+08:00"));
        assert!(value["unix_timestamp"].as_u64().is_some());
        assert!(value["unix_timestamp_ms"].as_u64().is_some());
        assert!(value["weekday"]["english"].as_str().is_some());
        assert!(value["weekday"]["chinese"].as_str().is_some());
        assert!(value["weekday"]["iso_number"].as_u64().is_some());
    }

    #[test]
    fn manage_yourself_parses_shell_like_command_without_shell() {
        let args =
            parse_manage_yourself_command("profile agent upsert dev './agents/coder profile.md'")
                .expect("quoted command should parse");

        assert_eq!(
            args,
            vec![
                "profile",
                "agent",
                "upsert",
                "dev",
                "./agents/coder profile.md"
            ]
        );
    }

    #[test]
    fn manage_yourself_rejects_empty_or_unclosed_quotes() {
        assert!(parse_manage_yourself_command("   ").is_err());
        assert!(parse_manage_yourself_command("profile 'unterminated").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn manage_yourself_formats_nonzero_exit_output() {
        use std::os::unix::process::ExitStatusExt;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(2 << 8),
            stdout: b"stdout\n".to_vec(),
            stderr: b"stderr\n".to_vec(),
        };

        assert_eq!(
            format_command_output(output),
            "stdout\n[stderr] stderr\n[exit 2]"
        );
    }

    async fn collect_tool_content(
        result: ToolResult<impl futures::Stream<Item = ToolOutput>>,
    ) -> Content {
        match result {
            ToolResult::Interrupt(_) => Content::text("interrupted"),
            ToolResult::Output(output) => {
                let mut output = std::pin::pin!(output);
                let mut last = Content::text(String::new());
                while let Some(item) = output.next().await {
                    if let ToolOutput::Result(content) = item {
                        last = content;
                    }
                }
                last
            }
        }
    }

    async fn collect_tool_text(
        result: ToolResult<impl futures::Stream<Item = ToolOutput>>,
    ) -> String {
        collect_tool_content(result).await.text_content()
    }

    #[tokio::test]
    async fn bash_tool_timeout_returns_pid_and_poll_completes() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        let tool = WorkspaceBashTool::new(
            Arc::new(NoSandbox::new(root.clone())),
            Arc::new(RwLock::new(SecretRedactor::empty())),
        );
        let ctx = test_tool_context();

        let started = collect_tool_text(
            <WorkspaceBashTool as Tool>::execute(
                &tool,
                json!({
                    "command": "printf start; sleep 0.2; printf done",
                    "timeout_ms": 10
                }),
                None,
                &ctx,
            )
            .await
            .expect("bash start should succeed"),
        )
        .await;
        let started: serde_json::Value =
            serde_json::from_str(&started).expect("timeout output should be json");
        assert_eq!(started["status"], "running");
        assert_eq!(started["timed_out"], true);
        let pid = started["pid"]
            .as_str()
            .expect("timeout output should include pid")
            .to_string();

        let mut combined_stdout = started["stdout"].as_str().unwrap_or_default().to_string();
        let mut completed = None;
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let polled = collect_tool_text(
                <WorkspaceBashTool as Tool>::execute(
                    &tool,
                    json!({ "pid": pid, "action": "poll" }),
                    None,
                    &ctx,
                )
                .await
                .expect("bash poll should succeed"),
            )
            .await;
            let polled: serde_json::Value =
                serde_json::from_str(&polled).expect("poll output should be json");
            combined_stdout.push_str(polled["stdout"].as_str().unwrap_or_default());
            if polled["status"] == "completed" {
                completed = Some(polled);
                break;
            }
        }

        let completed = completed.expect("bash task should complete");
        assert_eq!(completed["exit_code"], 0);
        assert!(combined_stdout.contains("start"));
        assert!(combined_stdout.contains("done"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn bash_tool_timeout_task_can_be_cancelled() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        let tool = WorkspaceBashTool::new(
            Arc::new(NoSandbox::new(root.clone())),
            Arc::new(RwLock::new(SecretRedactor::empty())),
        );
        let ctx = test_tool_context();

        let started = collect_tool_text(
            <WorkspaceBashTool as Tool>::execute(
                &tool,
                json!({ "command": "sleep 5", "timeout_ms": 10 }),
                None,
                &ctx,
            )
            .await
            .expect("bash start should succeed"),
        )
        .await;
        let started: serde_json::Value =
            serde_json::from_str(&started).expect("timeout output should be json");
        let pid = started["pid"]
            .as_str()
            .expect("timeout output should include pid")
            .to_string();

        let cancelled = collect_tool_text(
            <WorkspaceBashTool as Tool>::execute(
                &tool,
                json!({ "pid": pid, "action": "cancel" }),
                None,
                &ctx,
            )
            .await
            .expect("bash cancel should succeed"),
        )
        .await;
        let cancelled: serde_json::Value =
            serde_json::from_str(&cancelled).expect("cancel output should be json");
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["message"], "bash task cancelled");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn ssh_target_parser_validates_structured_target() {
        let target = parse_ssh_target(&json!({
            "host": "prod",
            "user": "deploy",
            "port": 2222
        }))
        .expect("valid ssh target should parse");
        assert_eq!(target.host, "prod");
        assert_eq!(target.user.as_deref(), Some("deploy"));
        assert_eq!(target.port, Some(2222));

        assert!(parse_ssh_target(&json!({ "host": "" })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "bad host" })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "-bad" })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "prod", "user": "bad user" })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "prod", "port": 0 })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "prod", "port": 70000 })).is_err());
    }

    #[test]
    fn ssh_command_args_are_structured_without_local_shell() {
        let target = SshTarget {
            host: "prod".to_string(),
            user: Some("deploy".to_string()),
            port: Some(2222),
        };
        assert_eq!(
            ssh_command_args(&target, Some("printf hello; pwd")),
            vec![
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-l",
                "deploy",
                "-p",
                "2222",
                "prod",
                "printf hello; pwd"
            ]
        );

        let target = SshTarget {
            host: "prod".to_string(),
            user: None,
            port: None,
        };
        assert_eq!(
            ssh_command_args(&target, None),
            vec![
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "prod"
            ]
        );
    }

    #[test]
    fn ssh_named_session_names_are_restricted() {
        assert_eq!(validate_ssh_named("alpha_1").unwrap(), "alpha_1");
        assert!(validate_ssh_named("").is_err());
        assert!(validate_ssh_named("bad/name").is_err());
        assert!(validate_ssh_named(&"a".repeat(65)).is_err());
    }

    #[tokio::test]
    async fn fs_read_returns_multimodal_content_for_png() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("tiny.png"), ONE_BY_ONE_PNG)
            .await
            .expect("png fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "tiny.png", "offset": 5, "length": 3 }),
                None,
                &test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await;

        match content {
            Content::Parts(parts) => {
                assert!(matches!(
                    parts.first(),
                    Some(remi_agentloop::prelude::ContentPart::Text { text })
                        if text.contains("mime_type=image/png")
                            && text.contains("offset/length are ignored")
                ));
                assert!(matches!(
                    parts.get(1),
                    Some(remi_agentloop::prelude::ContentPart::ImageUrl { image_url })
                        if image_url.url.starts_with("data:image/png;base64,")
                ));
            }
            other => panic!("expected multimodal content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fs_read_keeps_text_chunking_for_plain_text() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "hello world")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 6, "length": 5 }),
                None,
                &test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await;

        let text = content.text_content();
        assert!(text.contains("world"));
        assert!(text.contains("[offset=6 length=5 total_bytes=11 remaining=0]"));
    }

    #[tokio::test]
    async fn fs_read_warns_when_repeating_same_range_unchanged() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "hello world")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let first = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                &ctx,
            )
            .await
            .expect("first fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(!first.contains("repeats a range"));

        let second = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                &ctx,
            )
            .await
            .expect("second fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(second.contains("repeats a range already read"));
        assert!(second.contains("Continue with offset=5"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn fs_read_different_range_clears_previous_duplicate_marker() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "hello world")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let _ = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                &ctx,
            )
            .await
            .expect("first fs_read should succeed"),
        )
        .await;

        let next = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 6, "length": 5 }),
                None,
                &ctx,
            )
            .await
            .expect("next fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(!next.contains("repeats a range"));
        assert!(next.contains("world"));

        let repeated_next = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 6, "length": 5 }),
                None,
                &ctx,
            )
            .await
            .expect("repeated fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(repeated_next.contains("repeats a range already read"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn fs_read_same_range_after_mtime_change_does_not_warn() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        let path = root.join("note.txt");
        tokio::fs::write(&path, "hello world")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let _ = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                &ctx,
            )
            .await
            .expect("first fs_read should succeed"),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::fs::write(&path, "HELLO world")
            .await
            .expect("text fixture should be rewritten");

        let after_change = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                &ctx,
            )
            .await
            .expect("second fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(!after_change.contains("repeats a range"));
        assert!(after_change.contains("HELLO"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn fs_read_default_length_handles_larger_source_chunks() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        let body = "a".repeat(12_000);
        tokio::fs::write(root.join("large.rs"), &body)
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "large.rs" }),
                None,
                &test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await;

        let text = content.text_content();
        assert!(text.contains("[offset=0 length=12000 total_bytes=12000 remaining=0]"));
    }

    #[tokio::test]
    async fn fs_read_keeps_non_image_binary_on_text_path() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("blob.bin"), [0x00_u8, 0x9f, 0x92, 0x96])
            .await
            .expect("binary fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "blob.bin" }),
                None,
                &test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await;

        assert!(matches!(content, Content::Text(_)));
        assert!(content
            .text_content()
            .contains("[offset=0 length=4 total_bytes=4 remaining=0]"));
    }

    #[tokio::test]
    async fn apply_patch_updates_adds_and_deletes_multiple_files() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("a.txt"), "alpha\nbeta\n")
            .await
            .expect("a fixture should be written");
        tokio::fs::write(root.join("b.txt"), "remove me\n")
            .await
            .expect("b fixture should be written");

        let tool = RootedFsApplyPatchTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
        };
        let patch = r#"diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
-alpha
+ALPHA
 beta
diff --git a/c.txt b/c.txt
new file mode 100644
--- /dev/null
+++ b/c.txt
@@ -0,0 +1 @@
+created
diff --git a/b.txt b/b.txt
deleted file mode 100644
--- a/b.txt
+++ /dev/null
@@ -1 +0,0 @@
-remove me
"#;

        let content = collect_tool_content(
            <RootedFsApplyPatchTool as Tool>::execute(
                &tool,
                json!({ "patch": patch }),
                None,
                &test_tool_context(),
            )
            .await
            .expect("apply_patch should succeed"),
        )
        .await;

        assert!(content.text_content().contains("updated 1 file(s): a.txt"));
        assert_eq!(
            tokio::fs::read_to_string(root.join("a.txt")).await.unwrap(),
            "ALPHA\nbeta\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(root.join("c.txt")).await.unwrap(),
            "created\n"
        );
        assert!(!root.join("b.txt").exists());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn apply_patch_accepts_unprefixed_blank_context_lines() {
        let patch = r#"--- a/note.txt
+++ b/note.txt
@@ -1,3 +1,3 @@
-alpha
+ALPHA

 omega
"#;

        let ops = parse_apply_patch(patch).expect("blank context line should be tolerated");

        assert_eq!(
            ops,
            vec![ParsedPatchOp::Update {
                path: "note.txt".to_string(),
                hunks: vec![PatchHunk {
                    old: "alpha\n\nomega\n".to_string(),
                    new: "ALPHA\n\nomega\n".to_string(),
                }],
            }]
        );
    }

    #[test]
    fn apply_patch_reports_invalid_hunk_line_number_and_content() {
        let patch = r#"--- a/note.txt
+++ b/note.txt
@@ -1 +1 @@
 alpha
invalid
"#;

        let error = parse_apply_patch(patch).expect_err("invalid hunk line should fail");

        assert!(error.contains("patch line 5"), "{error}");
        assert!(error.contains("\"invalid\""), "{error}");
    }

    #[tokio::test]
    async fn apply_patch_rejects_ambiguous_update_hunks() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "same\nsame\n")
            .await
            .expect("fixture should be written");

        let tool = RootedFsApplyPatchTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
        };
        let patch = r#"diff --git a/note.txt b/note.txt
--- a/note.txt
+++ b/note.txt
@@ -1 +1 @@
-same
+changed
"#;

        let content = collect_tool_content(
            <RootedFsApplyPatchTool as Tool>::execute(
                &tool,
                json!({ "patch": patch }),
                None,
                &test_tool_context(),
            )
            .await
            .expect("apply_patch should return tool output"),
        )
        .await;

        assert!(content.text_content().contains("matched 2 times"));
        assert_eq!(
            tokio::fs::read_to_string(root.join("note.txt"))
                .await
                .unwrap(),
            "same\nsame\n"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn apply_patch_supports_standard_diff_relative_to_workdir() {
        let root = test_root();
        tokio::fs::create_dir_all(root.join("repo"))
            .await
            .expect("test repo should be created");
        tokio::fs::write(root.join("repo/app.py"), "print('old')\n")
            .await
            .expect("fixture should be written");

        let tool = RootedFsApplyPatchTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
        };
        let patch = r#"diff --git a/app.py b/app.py
--- a/app.py
+++ b/app.py
@@ -1 +1 @@
-print('old')
+print('new')
"#;

        let content = collect_tool_content(
            <RootedFsApplyPatchTool as Tool>::execute(
                &tool,
                json!({ "patch": patch, "workdir": "repo" }),
                None,
                &test_tool_context(),
            )
            .await
            .expect("apply_patch should succeed"),
        )
        .await;

        assert!(content
            .text_content()
            .contains("updated 1 file(s): repo/app.py"));
        assert_eq!(
            tokio::fs::read_to_string(root.join("repo/app.py"))
                .await
                .unwrap(),
            "print('new')\n"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
