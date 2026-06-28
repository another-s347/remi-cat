use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    AgentError, ResumePayload, Tool, ToolContext, ToolOutput, ToolResult,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command as TokioCommand};
use tokio::sync::Mutex;

use crate::sandbox::{SandboxBashOutput, SandboxBashStatus};

use super::{command_task_json, format_bash_text, json_text, log_preview, SharedRedactor};

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
pub(super) struct SshTarget {
    pub(super) host: String,
    pub(super) user: Option<String>,
    pub(super) port: Option<u16>,
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
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
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

pub(super) fn parse_ssh_target(arguments: &serde_json::Value) -> Result<SshTarget, String> {
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

pub(super) fn validate_ssh_named(name: &str) -> Result<String, String> {
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

pub(super) fn ssh_command_args(target: &SshTarget, command: Option<&str>) -> Vec<String> {
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
