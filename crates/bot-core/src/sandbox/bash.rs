use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::{SandboxBashOutput, SandboxBashStatus, BASH_TASK_RETAIN};

impl SandboxBashOutput {
    fn running(task: &mut BashTaskState, timed_out: bool) -> Self {
        let stdout = take_incremental(&task.stdout, &mut task.stdout_cursor);
        let stderr = take_incremental(&task.stderr, &mut task.stderr_cursor);
        Self {
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

    fn terminal(task: &mut BashTaskState, status: SandboxBashStatus) -> Self {
        let stdout = take_incremental(&task.stdout, &mut task.stdout_cursor);
        let stderr = take_incremental(&task.stderr, &mut task.stderr_cursor);
        Self {
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

    fn not_found(pid: &str) -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
            timed_out: false,
            status: SandboxBashStatus::NotFound,
            pid: Some(pid.to_string()),
            os_pid: None,
            message: Some("bash task not found; it may have completed and expired".to_string()),
        }
    }
}

#[derive(Debug)]
pub(super) struct BashSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BashTaskStatus {
    Running,
    Completed,
    Cancelled,
}

#[derive(Debug)]
struct BashTaskState {
    pid: String,
    named: Option<String>,
    stdout: String,
    stderr: String,
    stdout_cursor: usize,
    stderr_cursor: usize,
    os_pid: Option<u32>,
    exit_code: Option<i32>,
    status: BashTaskStatus,
    message: Option<String>,
    finished_at: Option<Instant>,
}

#[derive(Clone, Debug)]
pub(super) struct BashTaskRegistry {
    tasks: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<BashTaskState>>>>>,
}

impl BashTaskRegistry {
    pub(super) fn new() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    async fn insert(&self, state: BashTaskState) -> Arc<Mutex<BashTaskState>> {
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
            if task.status == BashTaskStatus::Running && task.named.as_deref() == Some(named) {
                return Some(task.pid.clone());
            }
        }
        None
    }

    pub(super) async fn poll(&self, pid: &str) -> SandboxBashOutput {
        self.prune().await;
        let task = self.tasks.lock().await.get(pid).cloned();
        let Some(task) = task else {
            return SandboxBashOutput::not_found(pid);
        };
        let mut task = task.lock().await;
        match task.status {
            BashTaskStatus::Running => SandboxBashOutput::running(&mut task, false),
            BashTaskStatus::Completed => {
                SandboxBashOutput::terminal(&mut task, SandboxBashStatus::Completed)
            }
            BashTaskStatus::Cancelled => {
                SandboxBashOutput::terminal(&mut task, SandboxBashStatus::Cancelled)
            }
        }
    }

    pub(super) async fn cancel(&self, pid: &str) -> SandboxBashOutput {
        self.prune().await;
        let task = self.tasks.lock().await.get(pid).cloned();
        let Some(task) = task else {
            return SandboxBashOutput::not_found(pid);
        };
        let os_pid = {
            let mut task = task.lock().await;
            if task.status == BashTaskStatus::Running {
                task.status = BashTaskStatus::Cancelled;
                task.exit_code = Some(-1);
                task.message = Some("bash task cancelled".to_string());
                task.finished_at = Some(Instant::now());
            }
            task.os_pid
        };
        if let Some(os_pid) = os_pid {
            terminate_process(os_pid).await;
        }
        let mut task = task.lock().await;
        SandboxBashOutput::terminal(&mut task, SandboxBashStatus::Cancelled)
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
                .map(|finished_at| finished_at.elapsed() >= BASH_TASK_RETAIN)
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

impl BashSession {
    pub(super) async fn start_local(root: &Path) -> Result<Self> {
        let shell = user_shell();
        let child = Command::new(&shell)
            .args(shell_session_args(&shell))
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting named bash session")?;
        let mut session = Self::from_child(child, "named bash session")?;
        session.discard_startup_output(&shell).await?;
        Ok(session)
    }

    pub(super) async fn start_docker(
        container_name: &str,
        container_dir: &str,
        user: Option<&str>,
    ) -> Result<Self> {
        let mut cmd = Command::new("docker");
        cmd.args(["exec", "-i", "-w", container_dir]);
        if let Some(user) = user {
            cmd.args(["-u", user]);
        }
        let child = cmd
            .args([
                container_name,
                "bash",
                "-l",
                "-c",
                "exec 2>&1; exec bash -l",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting named bash session")?;
        let mut session = Self::from_child(child, "named bash session")?;
        session.discard_startup_output("bash").await?;
        Ok(session)
    }

    fn from_child(mut child: Child, label: &str) -> Result<Self> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("{label} stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("{label} stdout unavailable"))?;
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
        task: Arc<Mutex<BashTaskState>>,
    ) -> Result<i32> {
        let marker = format!("__REMI_BASH_DONE_{}__", uuid::Uuid::new_v4().simple());
        let wrapped = format!(
            "{command} 2>&1\n__remi_status=$?\nprintf '\\n{marker}:%s\\n' \"$__remi_status\"\n"
        );
        self.stdin
            .write_all(wrapped.as_bytes())
            .await
            .context("writing to docker bash session")?;
        self.stdin
            .flush()
            .await
            .context("flushing docker bash session")?;

        let mut line = String::new();
        loop {
            line.clear();
            let n = self
                .stdout
                .read_line(&mut line)
                .await
                .context("reading docker bash session")?;
            if n == 0 {
                return Err(anyhow!("named bash session exited"));
            }
            if let Some(exit_code) = parse_marker_line(&line, &marker) {
                trim_task_stdout_newline(&task).await;
                return Ok(exit_code);
            }
            append_task_stdout(&task, &line).await;
        }
    }

    async fn discard_startup_output(&mut self, shell: &str) -> Result<()> {
        let marker = format!("__REMI_BASH_READY_{}__", uuid::Uuid::new_v4().simple());
        let probe = format!("{}\nprintf '\\n{marker}\\n'\n", shell_startup_script(shell));
        self.stdin
            .write_all(probe.as_bytes())
            .await
            .context("writing named bash startup probe")?;
        self.stdin
            .flush()
            .await
            .context("flushing named bash startup probe")?;

        let mut line = String::new();
        loop {
            line.clear();
            let n = self
                .stdout
                .read_line(&mut line)
                .await
                .context("reading named bash startup probe")?;
            if n == 0 {
                return Err(anyhow!("named bash session exited during startup"));
            }
            if line.trim_end() == marker {
                return Ok(());
            }
        }
    }
}

pub(super) fn user_shell() -> String {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "bash".to_string());
    match shell_name(&shell) {
        "bash" | "zsh" => shell,
        _ => "bash".to_string(),
    }
}

fn shell_name(shell: &str) -> &str {
    Path::new(shell)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(shell)
}

pub(super) fn shell_command_args(shell: &str, command: &str) -> Vec<String> {
    let command = format!("{}\n{command}", shell_startup_script(shell));
    match shell_name(shell) {
        "bash" | "zsh" => vec!["-l".to_string(), "-c".to_string(), command],
        _ => vec!["-c".to_string(), command],
    }
}

fn shell_session_args(shell: &str) -> Vec<&'static str> {
    match shell_name(shell) {
        "bash" | "zsh" => vec!["-l"],
        _ => Vec::new(),
    }
}

pub(super) fn shell_startup_script(shell: &str) -> &'static str {
    match shell_name(shell) {
        "zsh" => {
            r#"if [ -f "$HOME/.zshrc" ]; then . "$HOME/.zshrc"; fi
precmd_functions=()
preexec_functions=()
chpwd_functions=()
periodic_functions=()
PROMPT=
RPROMPT=
unsetopt xtrace 2>/dev/null || true"#
        }
        "bash" => {
            r#"if [ -f "$HOME/.bashrc" ]; then . "$HOME/.bashrc"; fi
PROMPT_COMMAND=
trap - DEBUG 2>/dev/null || true
set +x"#
        }
        _ => "",
    }
}

pub(super) async fn run_command_with_timeout(
    mut cmd: Command,
    timeout_ms: u64,
    named: Option<String>,
    tasks: BashTaskRegistry,
) -> Result<SandboxBashOutput> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("running sandbox command")?;
    let os_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("sandbox command stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("sandbox command stderr unavailable"))?;
    let task = tasks
        .insert(BashTaskState {
            pid: new_bash_pid(),
            named,
            stdout: String::new(),
            stderr: String::new(),
            stdout_cursor: 0,
            stderr_cursor: 0,
            os_pid,
            exit_code: None,
            status: BashTaskStatus::Running,
            message: None,
            finished_at: None,
        })
        .await;
    spawn_reader(stdout, Arc::clone(&task), true);
    spawn_reader(stderr, Arc::clone(&task), false);
    tokio::spawn({
        let wait_task = Arc::clone(&task);
        async move {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let mut task = wait_task.lock().await;
                        if task.status == BashTaskStatus::Running {
                            task.exit_code = Some(status.code().unwrap_or(-1));
                            task.status = BashTaskStatus::Completed;
                            task.finished_at = Some(Instant::now());
                        }
                        break;
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Err(err) => {
                        let mut task = wait_task.lock().await;
                        if task.status == BashTaskStatus::Running {
                            task.exit_code = Some(-1);
                            task.status = BashTaskStatus::Completed;
                            task.message = Some(format!("bash wait failed: {err}"));
                            task.finished_at = Some(Instant::now());
                        }
                        break;
                    }
                }
            }
            let _ = child.wait().await;
        }
    });
    wait_for_task(task, timeout_ms).await
}

pub(super) async fn run_named_bash<F, Fut>(
    sessions: &Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<BashSession>>>>>,
    tasks: BashTaskRegistry,
    named: &str,
    start: F,
    command: &str,
    timeout_ms: u64,
) -> Result<SandboxBashOutput>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<BashSession>>,
{
    let named = validate_named(named)?;
    if let Some(pid) = tasks.active_named_pid(&named).await {
        return Err(anyhow!(
            "named bash session `{named}` is still running task `{pid}`; poll or cancel that pid, wait for it to complete, or use a different named session"
        ));
    }
    let session = {
        let mut sessions = sessions.lock().await;
        if let Some(session) = sessions.get(&named) {
            Arc::clone(session)
        } else {
            let session = Arc::new(Mutex::new(start().await?));
            sessions.insert(named.clone(), Arc::clone(&session));
            session
        }
    };

    let task = tasks
        .insert(BashTaskState {
            pid: new_bash_pid(),
            named: Some(named.clone()),
            stdout: String::new(),
            stderr: String::new(),
            stdout_cursor: 0,
            stderr_cursor: 0,
            os_pid: None,
            exit_code: None,
            status: BashTaskStatus::Running,
            message: None,
            finished_at: None,
        })
        .await;
    let sessions = Arc::clone(sessions);
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
                    BashTaskStatus::Cancelled => {
                        remove_session = true;
                    }
                    BashTaskStatus::Running => match result {
                        Ok(exit_code) => {
                            task.exit_code = Some(exit_code);
                            task.status = BashTaskStatus::Completed;
                            task.finished_at = Some(Instant::now());
                        }
                        Err(err) => {
                            task.exit_code = Some(-1);
                            task.status = BashTaskStatus::Completed;
                            task.message = Some(err.to_string());
                            task.finished_at = Some(Instant::now());
                            remove_session = true;
                        }
                    },
                    BashTaskStatus::Completed => {}
                }
            }
            if remove_session {
                sessions.lock().await.remove(&named);
            }
        }
    });
    wait_for_task(task, timeout_ms).await
}

async fn wait_for_task(
    task: Arc<Mutex<BashTaskState>>,
    timeout_ms: u64,
) -> Result<SandboxBashOutput> {
    let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout);
    loop {
        {
            let mut task = task.lock().await;
            match task.status {
                BashTaskStatus::Running => {}
                BashTaskStatus::Completed => {
                    let mut output =
                        SandboxBashOutput::terminal(&mut task, SandboxBashStatus::Completed);
                    output.pid = None;
                    output.os_pid = None;
                    return Ok(output);
                }
                BashTaskStatus::Cancelled => {
                    return Ok(SandboxBashOutput::terminal(
                        &mut task,
                        SandboxBashStatus::Cancelled,
                    ));
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            _ = &mut timeout => {
                let mut task = task.lock().await;
                return Ok(SandboxBashOutput::running(&mut task, true));
            }
        }
    }
}

fn new_bash_pid() -> String {
    format!("bash_{}", uuid::Uuid::new_v4().simple())
}

fn take_incremental(buffer: &str, cursor: &mut usize) -> String {
    let cursor_value = (*cursor).min(buffer.len());
    let out = buffer[cursor_value..].to_string();
    *cursor = buffer.len();
    out
}

fn spawn_reader<R>(mut reader: R, task: Arc<Mutex<BashTaskState>>, stdout: bool)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if stdout {
                        append_task_stdout(&task, &chunk).await;
                    } else {
                        append_task_stderr(&task, &chunk).await;
                    }
                }
                Err(err) => {
                    append_task_stderr(&task, &format!("\n[read error: {err}]")).await;
                    break;
                }
            }
        }
    });
}

async fn append_task_stdout(task: &Arc<Mutex<BashTaskState>>, chunk: &str) {
    task.lock().await.stdout.push_str(chunk);
}

async fn append_task_stderr(task: &Arc<Mutex<BashTaskState>>, chunk: &str) {
    task.lock().await.stderr.push_str(chunk);
}

async fn trim_task_stdout_newline(task: &Arc<Mutex<BashTaskState>>) {
    let mut task = task.lock().await;
    if task.stdout.ends_with('\n') {
        task.stdout.pop();
        if task.stdout.ends_with('\r') {
            task.stdout.pop();
        }
    }
}

async fn terminate_process(pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .output()
            .await;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output()
            .await;
    }
}

fn validate_named(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("named bash session may not be empty"));
    }
    if trimmed.len() > 64 {
        return Err(anyhow!("named bash session must be at most 64 characters"));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "named bash session may only contain ASCII letters, numbers, '-' and '_'"
        ));
    }
    Ok(trimmed.to_string())
}

fn parse_marker_line(line: &str, marker: &str) -> Option<i32> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let rest = trimmed.strip_prefix(marker)?.strip_prefix(':')?;
    rest.parse().ok()
}
