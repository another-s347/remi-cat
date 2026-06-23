use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

pub type SandboxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

const BASH_TASK_RETAIN: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxBashStatus {
    Completed,
    Running,
    Cancelled,
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxBashOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub status: SandboxBashStatus,
    pub pid: Option<String>,
    pub os_pid: Option<u32>,
    pub message: Option<String>,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxConfig {
    Disabled { host_dir: PathBuf },
    NoSandbox { host_dir: PathBuf },
    Docker(DockerSandboxConfig),
}

impl SandboxConfig {
    pub fn from_env(data_dir: PathBuf, bash_mode: crate::tools::BashMode) -> Self {
        let kind = std::env::var("REMI_SANDBOX_KIND")
            .ok()
            .map(|value| value.trim().to_ascii_lowercase());
        let host_dir = std::env::var("REMI_SANDBOX_HOST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.clone());
        match kind.as_deref() {
            Some("docker") => Self::Docker(DockerSandboxConfig::from_env(host_dir)),
            Some("no_sandbox") | Some("no-sandbox") | Some("local") => Self::NoSandbox { host_dir },
            Some("disabled") => Self::Disabled { host_dir },
            _ => match bash_mode {
                crate::tools::BashMode::Local => Self::NoSandbox { host_dir },
                crate::tools::BashMode::Docker => Self::Disabled { host_dir },
            },
        }
    }

    pub fn bash_enabled(&self) -> bool {
        !matches!(self, Self::Disabled { .. })
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Disabled { .. } => "disabled",
            Self::NoSandbox { .. } => "no_sandbox",
            Self::Docker(_) => "docker",
        }
    }

    pub fn host_dir(&self) -> &Path {
        match self {
            Self::Disabled { host_dir } | Self::NoSandbox { host_dir } => host_dir,
            Self::Docker(config) => &config.host_dir,
        }
    }

    pub fn workspace_root_label(&self) -> String {
        match self {
            Self::Disabled { host_dir } | Self::NoSandbox { host_dir } => {
                host_dir.display().to_string()
            }
            Self::Docker(config) => config.container_dir.clone(),
        }
    }

    pub fn build(&self) -> Result<std::sync::Arc<dyn Sandbox>> {
        match self {
            Self::Disabled { host_dir } | Self::NoSandbox { host_dir } => {
                Ok(std::sync::Arc::new(NoSandbox::new(host_dir.clone())))
            }
            Self::Docker(config) => Ok(std::sync::Arc::new(DockerSandbox::new(config.clone()))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerSandboxConfig {
    pub host_dir: PathBuf,
    pub container_dir: String,
    pub image: String,
    pub container_name: String,
    pub user: Option<String>,
}

impl DockerSandboxConfig {
    pub fn from_env(host_dir: PathBuf) -> Self {
        Self {
            host_dir,
            container_dir: std::env::var("REMI_SANDBOX_CONTAINER_DIR")
                .unwrap_or_else(|_| "/workspace".to_string()),
            image: std::env::var("REMI_SANDBOX_IMAGE")
                .unwrap_or_else(|_| "mcr.microsoft.com/devcontainers/base:bookworm".to_string()),
            container_name: std::env::var("REMI_SANDBOX_CONTAINER_NAME")
                .unwrap_or_else(|_| "remi-cat-sandbox".to_string()),
            user: std::env::var("REMI_SANDBOX_USER")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(current_host_user_spec),
        }
    }
}

pub trait Sandbox: Send + Sync {
    fn kind(&self) -> &'static str;
    fn workspace_root_label(&self) -> String;
    fn read<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, Vec<u8>>;
    fn metadata<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, SandboxMetadata>;
    fn write<'a>(&'a self, path: &'a str, content: &'a [u8]) -> SandboxFuture<'a, ()>;
    fn replace<'a>(
        &'a self,
        path: &'a str,
        old: &'a str,
        new: &'a str,
    ) -> SandboxFuture<'a, ReplaceResult>;
    fn mkdir<'a>(&'a self, path: &'a str, recursive: bool) -> SandboxFuture<'a, ()>;
    fn remove<'a>(&'a self, path: &'a str, recursive: bool) -> SandboxFuture<'a, ()>;
    fn list<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, Vec<String>>;
    fn bash<'a>(
        &'a self,
        command: &'a str,
        named: Option<&'a str>,
        timeout_ms: u64,
    ) -> SandboxFuture<'a, SandboxBashOutput>;
    fn bash_poll<'a>(&'a self, pid: &'a str) -> SandboxFuture<'a, SandboxBashOutput>;
    fn bash_cancel<'a>(&'a self, pid: &'a str) -> SandboxFuture<'a, SandboxBashOutput>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxMetadata {
    pub len: u64,
    pub modified_ms: Option<u64>,
}

fn sandbox_metadata_from_std(metadata: std::fs::Metadata) -> SandboxMetadata {
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64);
    SandboxMetadata {
        len: metadata.len(),
        modified_ms,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceResult {
    Replaced,
    NotFound,
    MultipleMatches(usize),
}

#[derive(Debug, Clone)]
pub struct NoSandbox {
    root: PathBuf,
    sessions: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<BashSession>>>>>,
    tasks: BashTaskRegistry,
}

impl NoSandbox {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
            tasks: BashTaskRegistry::new(),
        }
    }

    async fn bash_once(&self, command: &str, timeout_ms: u64) -> Result<SandboxBashOutput> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .context("creating sandbox root")?;
        let shell = user_shell();
        let mut cmd = Command::new(&shell);
        cmd.args(shell_command_args(&shell, command));
        cmd.current_dir(&self.root);
        run_command_with_timeout(cmd, timeout_ms, None, self.tasks.clone()).await
    }

    async fn bash_named(
        &self,
        named: &str,
        command: &str,
        timeout_ms: u64,
    ) -> Result<SandboxBashOutput> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .context("creating sandbox root")?;
        run_named_bash(
            &self.sessions,
            self.tasks.clone(),
            named,
            || BashSession::start_local(&self.root),
            command,
            timeout_ms,
        )
        .await
    }
}

impl Sandbox for NoSandbox {
    fn kind(&self) -> &'static str {
        "no_sandbox"
    }

    fn workspace_root_label(&self) -> String {
        self.root.display().to_string()
    }

    fn read<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let full = resolve_local_existing_path(&self.root, path).await?;
            tokio::fs::read(full).await.context("reading local file")
        })
    }

    fn metadata<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, SandboxMetadata> {
        Box::pin(async move {
            let full = resolve_local_existing_path(&self.root, path).await?;
            let metadata = tokio::fs::metadata(full)
                .await
                .context("reading local file metadata")?;
            Ok(sandbox_metadata_from_std(metadata))
        })
    }

    fn write<'a>(&'a self, path: &'a str, content: &'a [u8]) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let full = resolve_local_path(&self.root, path).await?;
            tokio::fs::write(full, content)
                .await
                .context("writing local file")
        })
    }

    fn replace<'a>(
        &'a self,
        path: &'a str,
        old: &'a str,
        new: &'a str,
    ) -> SandboxFuture<'a, ReplaceResult> {
        Box::pin(async move {
            let full = resolve_local_existing_path(&self.root, path).await?;
            let content = tokio::fs::read_to_string(&full)
                .await
                .context("reading local file")?;
            let count = content.matches(old).count();
            if count == 0 {
                return Ok(ReplaceResult::NotFound);
            }
            if count > 1 {
                return Ok(ReplaceResult::MultipleMatches(count));
            }
            let replaced = content.replacen(old, new, 1);
            tokio::fs::write(full, replaced.as_bytes())
                .await
                .context("writing local file")?;
            Ok(ReplaceResult::Replaced)
        })
    }

    fn mkdir<'a>(&'a self, path: &'a str, recursive: bool) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let full = resolve_local_path(&self.root, path).await?;
            if recursive {
                tokio::fs::create_dir_all(full).await
            } else {
                tokio::fs::create_dir(full).await
            }
            .context("creating local directory")
        })
    }

    fn remove<'a>(&'a self, path: &'a str, recursive: bool) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let full = resolve_local_existing_path(&self.root, path).await?;
            if recursive {
                tokio::fs::remove_dir_all(full).await
            } else if tokio::fs::metadata(&full)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                tokio::fs::remove_dir(full).await
            } else {
                tokio::fs::remove_file(full).await
            }
            .context("removing local path")
        })
    }

    fn list<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, Vec<String>> {
        Box::pin(async move {
            let full = resolve_local_existing_path(&self.root, path).await?;
            let mut rd = tokio::fs::read_dir(full)
                .await
                .context("listing local directory")?;
            let mut entries = Vec::new();
            while let Some(entry) = rd.next_entry().await.context("reading sandbox directory")? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let suffix = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    "/"
                } else {
                    ""
                };
                entries.push(format!("{name}{suffix}"));
            }
            entries.sort();
            Ok(entries)
        })
    }

    fn bash<'a>(
        &'a self,
        command: &'a str,
        named: Option<&'a str>,
        timeout_ms: u64,
    ) -> SandboxFuture<'a, SandboxBashOutput> {
        Box::pin(async move {
            match named.map(str::trim).filter(|value| !value.is_empty()) {
                Some(named) => self.bash_named(named, command, timeout_ms).await,
                None => self.bash_once(command, timeout_ms).await,
            }
        })
    }

    fn bash_poll<'a>(&'a self, pid: &'a str) -> SandboxFuture<'a, SandboxBashOutput> {
        Box::pin(async move { Ok(self.tasks.poll(pid).await) })
    }

    fn bash_cancel<'a>(&'a self, pid: &'a str) -> SandboxFuture<'a, SandboxBashOutput> {
        Box::pin(async move { Ok(self.tasks.cancel(pid).await) })
    }
}

#[derive(Clone)]
pub struct DockerSandbox {
    config: DockerSandboxConfig,
    sessions: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<BashSession>>>>>,
    tasks: BashTaskRegistry,
}

impl DockerSandbox {
    pub fn new(config: DockerSandboxConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
            tasks: BashTaskRegistry::new(),
        }
    }

    async fn ensure_running(&self) -> Result<()> {
        let host_dir = absolute_existing_dir(&self.config.host_dir).await?;
        let status = Command::new("docker")
            .args([
                "inspect",
                "-f",
                "{{.State.Running}}",
                &self.config.container_name,
            ])
            .output()
            .await;
        match status {
            Ok(output) if output.status.success() => {
                if String::from_utf8_lossy(&output.stdout).trim() == "true" {
                    return Ok(());
                }
                let start = Command::new("docker")
                    .args(["start", &self.config.container_name])
                    .output()
                    .await
                    .context("starting docker sandbox container")?;
                if start.status.success() {
                    return Ok(());
                }
                return Err(anyhow!(
                    "docker start failed: {}",
                    String::from_utf8_lossy(&start.stderr).trim()
                ));
            }
            _ => {}
        }

        let mount = format!("{}:{}", host_dir.display(), self.config.container_dir);
        let mut run_cmd = Command::new("docker");
        run_cmd.args(["run", "-d", "--name", &self.config.container_name]);
        if let Some(user) = self.config.user.as_deref() {
            run_cmd.args(["-u", user]);
        }
        let run = run_cmd
            .args([
                "-v",
                &mount,
                "-w",
                &self.config.container_dir,
                &self.config.image,
                "sleep",
                "infinity",
            ])
            .output()
            .await
            .context("creating docker sandbox container")?;
        if run.status.success() {
            return Ok(());
        }
        Err(anyhow!(
            "docker run failed: {}",
            String::from_utf8_lossy(&run.stderr).trim()
        ))
    }

    async fn bash_once(&self, command: &str, timeout_ms: u64) -> Result<SandboxBashOutput> {
        self.ensure_running().await?;
        let mut cmd = Command::new("docker");
        cmd.args(["exec", "-w", &self.config.container_dir]);
        if let Some(user) = self.config.user.as_deref() {
            cmd.args(["-u", user]);
        }
        let command = format!("{}\n{command}", shell_startup_script("bash"));
        cmd.args([&self.config.container_name, "bash", "-l", "-c", &command]);
        run_command_with_timeout(cmd, timeout_ms, None, self.tasks.clone()).await
    }

    async fn bash_session(
        &self,
        named: &str,
        command: &str,
        timeout_ms: u64,
    ) -> Result<SandboxBashOutput> {
        self.ensure_running().await?;
        let container_name = self.config.container_name.clone();
        let container_dir = self.config.container_dir.clone();
        let user = self.config.user.clone();
        run_named_bash(
            &self.sessions,
            self.tasks.clone(),
            named,
            || BashSession::start_docker(&container_name, &container_dir, user.as_deref()),
            command,
            timeout_ms,
        )
        .await
    }
}

impl Sandbox for DockerSandbox {
    fn kind(&self) -> &'static str {
        "docker"
    }

    fn workspace_root_label(&self) -> String {
        self.config.container_dir.clone()
    }

    fn read<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let full = resolve_workspace_existing_path(
                &self.config.host_dir,
                &self.config.container_dir,
                path,
            )
            .await?;
            tokio::fs::read(full).await.context("reading sandbox file")
        })
    }

    fn metadata<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, SandboxMetadata> {
        Box::pin(async move {
            let full = resolve_workspace_existing_path(
                &self.config.host_dir,
                &self.config.container_dir,
                path,
            )
            .await?;
            let metadata = tokio::fs::metadata(full)
                .await
                .context("reading sandbox file metadata")?;
            Ok(sandbox_metadata_from_std(metadata))
        })
    }

    fn write<'a>(&'a self, path: &'a str, content: &'a [u8]) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let full = resolve_workspace_writable_file_path(
                &self.config.host_dir,
                &self.config.container_dir,
                path,
            )
            .await?;
            tokio::fs::write(full, content)
                .await
                .context("writing sandbox file")
        })
    }

    fn replace<'a>(
        &'a self,
        path: &'a str,
        old: &'a str,
        new: &'a str,
    ) -> SandboxFuture<'a, ReplaceResult> {
        Box::pin(async move {
            let full = resolve_workspace_existing_path(
                &self.config.host_dir,
                &self.config.container_dir,
                path,
            )
            .await?;
            let content = tokio::fs::read_to_string(&full)
                .await
                .context("reading sandbox file")?;
            let count = content.matches(old).count();
            if count == 0 {
                return Ok(ReplaceResult::NotFound);
            }
            if count > 1 {
                return Ok(ReplaceResult::MultipleMatches(count));
            }
            let replaced = content.replacen(old, new, 1);
            tokio::fs::write(full, replaced.as_bytes())
                .await
                .context("writing sandbox file")?;
            Ok(ReplaceResult::Replaced)
        })
    }

    fn mkdir<'a>(&'a self, path: &'a str, recursive: bool) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let full = resolve_workspace_writable_dir_path(
                &self.config.host_dir,
                &self.config.container_dir,
                path,
                recursive,
            )
            .await?;
            if recursive {
                tokio::fs::create_dir_all(full).await
            } else {
                tokio::fs::create_dir(full).await
            }
            .context("creating sandbox directory")
        })
    }

    fn remove<'a>(&'a self, path: &'a str, recursive: bool) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let full = resolve_workspace_existing_path(
                &self.config.host_dir,
                &self.config.container_dir,
                path,
            )
            .await?;
            if recursive {
                tokio::fs::remove_dir_all(full).await
            } else if tokio::fs::metadata(&full)
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false)
            {
                tokio::fs::remove_dir(full).await
            } else {
                tokio::fs::remove_file(full).await
            }
            .context("removing sandbox path")
        })
    }

    fn list<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, Vec<String>> {
        Box::pin(async move {
            let full = resolve_workspace_existing_path(
                &self.config.host_dir,
                &self.config.container_dir,
                path,
            )
            .await?;
            let mut rd = tokio::fs::read_dir(full)
                .await
                .context("listing sandbox directory")?;
            let mut entries = Vec::new();
            while let Some(entry) = rd.next_entry().await.context("reading sandbox directory")? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let suffix = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                    "/"
                } else {
                    ""
                };
                entries.push(format!("{name}{suffix}"));
            }
            entries.sort();
            Ok(entries)
        })
    }

    fn bash<'a>(
        &'a self,
        command: &'a str,
        named: Option<&'a str>,
        timeout_ms: u64,
    ) -> SandboxFuture<'a, SandboxBashOutput> {
        Box::pin(async move {
            match named.map(str::trim).filter(|value| !value.is_empty()) {
                Some(named) => self.bash_session(named, command, timeout_ms).await,
                None => self.bash_once(command, timeout_ms).await,
            }
        })
    }

    fn bash_poll<'a>(&'a self, pid: &'a str) -> SandboxFuture<'a, SandboxBashOutput> {
        Box::pin(async move { Ok(self.tasks.poll(pid).await) })
    }

    fn bash_cancel<'a>(&'a self, pid: &'a str) -> SandboxFuture<'a, SandboxBashOutput> {
        Box::pin(async move { Ok(self.tasks.cancel(pid).await) })
    }
}

#[derive(Debug)]
struct BashSession {
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
struct BashTaskRegistry {
    tasks: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<BashTaskState>>>>>,
}

impl BashTaskRegistry {
    fn new() -> Self {
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

    async fn poll(&self, pid: &str) -> SandboxBashOutput {
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

    async fn cancel(&self, pid: &str) -> SandboxBashOutput {
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
    async fn start_local(root: &Path) -> Result<Self> {
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

    async fn start_docker(
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

fn user_shell() -> String {
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

fn shell_command_args(shell: &str, command: &str) -> Vec<String> {
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

fn shell_startup_script(shell: &str) -> &'static str {
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

async fn run_command_with_timeout(
    mut cmd: Command,
    timeout_ms: u64,
    named: Option<String>,
    tasks: BashTaskRegistry,
) -> Result<SandboxBashOutput> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
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

async fn run_named_bash<F, Fut>(
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

async fn absolute_existing_dir(path: &Path) -> Result<PathBuf> {
    tokio::fs::create_dir_all(path)
        .await
        .with_context(|| format!("creating sandbox dir {}", path.display()))?;
    tokio::fs::canonicalize(path)
        .await
        .with_context(|| format!("canonicalizing sandbox dir {}", path.display()))
}

fn clean_workspace_path(path: &str, workspace_root_label: &str) -> Result<PathBuf> {
    let input = Path::new(path);
    if input.is_absolute() {
        let label = Path::new(workspace_root_label);
        let stripped = input.strip_prefix(label).with_context(|| {
            format!("absolute sandbox path must start with {}", label.display())
        })?;
        return clean_relative_components(stripped);
    }
    clean_relative_components(input)
}

fn clean_relative_components(path: &Path) -> Result<PathBuf> {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => cleaned.push(part),
            Component::CurDir => {}
            Component::ParentDir => return Err(anyhow!("path may not contain '..'")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("path must be relative to the sandbox root"))
            }
        }
    }
    if cleaned.as_os_str().is_empty() {
        Ok(PathBuf::from("."))
    } else {
        Ok(cleaned)
    }
}

async fn resolve_local_path(root: &Path, path: &str) -> Result<PathBuf> {
    tokio::fs::create_dir_all(root)
        .await
        .with_context(|| format!("creating local root {}", root.display()))?;
    let input = Path::new(path);
    if input.is_absolute() {
        Ok(input.to_path_buf())
    } else {
        Ok(root.join(input))
    }
}

async fn resolve_local_existing_path(root: &Path, path: &str) -> Result<PathBuf> {
    let full = resolve_local_path(root, path).await?;
    tokio::fs::canonicalize(&full)
        .await
        .with_context(|| format!("canonicalizing local path {}", path))
}

async fn canonical_root(root: &Path) -> Result<PathBuf> {
    tokio::fs::create_dir_all(root)
        .await
        .with_context(|| format!("creating sandbox root {}", root.display()))?;
    tokio::fs::canonicalize(root)
        .await
        .with_context(|| format!("canonicalizing sandbox root {}", root.display()))
}

fn current_host_user_spec() -> Option<String> {
    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let gid = std::process::Command::new("id")
        .arg("-g")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    Some(format!("{uid}:{gid}"))
}

async fn resolve_workspace_existing_path(
    root: &Path,
    workspace_root_label: &str,
    path: &str,
) -> Result<PathBuf> {
    let root = canonical_root(root).await?;
    let relative = clean_workspace_path(path, workspace_root_label)?;
    resolve_existing_relative_path(&root, &relative, path).await
}

async fn resolve_existing_relative_path(
    root: &Path,
    relative: &Path,
    original_path: &str,
) -> Result<PathBuf> {
    let full = root.join(relative);
    let canonical = tokio::fs::canonicalize(&full)
        .await
        .with_context(|| format!("canonicalizing sandbox path {}", original_path))?;
    if !canonical.starts_with(&root) {
        return Err(anyhow!("path escapes sandbox root"));
    }
    Ok(canonical)
}

async fn resolve_workspace_writable_file_path(
    root: &Path,
    workspace_root_label: &str,
    path: &str,
) -> Result<PathBuf> {
    let root = canonical_root(root).await?;
    let relative = clean_workspace_path(path, workspace_root_label)?;
    resolve_writable_file_relative_path(&root, &relative, path).await
}

async fn resolve_writable_file_relative_path(
    root: &Path,
    relative: &Path,
    original_path: &str,
) -> Result<PathBuf> {
    if relative == Path::new(".") {
        return Err(anyhow!("path must refer to a file"));
    }
    let full = root.join(relative);
    if tokio::fs::symlink_metadata(&full).await.is_ok() {
        let canonical = tokio::fs::canonicalize(&full)
            .await
            .with_context(|| format!("canonicalizing sandbox path {}", original_path))?;
        if !canonical.starts_with(&root) {
            return Err(anyhow!("path escapes sandbox root"));
        }
        return Ok(canonical);
    }
    let parent = full
        .parent()
        .ok_or_else(|| anyhow!("path has no parent directory"))?;
    let parent = tokio::fs::canonicalize(parent)
        .await
        .with_context(|| format!("canonicalizing parent for sandbox path {}", original_path))?;
    if !parent.starts_with(&root) {
        return Err(anyhow!("path escapes sandbox root"));
    }
    Ok(full)
}

async fn resolve_workspace_writable_dir_path(
    root: &Path,
    workspace_root_label: &str,
    path: &str,
    recursive: bool,
) -> Result<PathBuf> {
    let root = canonical_root(root).await?;
    let relative = clean_workspace_path(path, workspace_root_label)?;
    let full = root.join(&relative);
    if tokio::fs::symlink_metadata(&full).await.is_ok() {
        let canonical = tokio::fs::canonicalize(&full)
            .await
            .with_context(|| format!("canonicalizing sandbox path {}", path))?;
        if !canonical.starts_with(&root) {
            return Err(anyhow!("path escapes sandbox root"));
        }
        return Ok(canonical);
    }

    let ancestor = if recursive {
        nearest_existing_ancestor(&full)
    } else {
        full.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("path has no parent directory"))?
    };
    let ancestor = tokio::fs::canonicalize(&ancestor)
        .await
        .with_context(|| format!("canonicalizing parent for sandbox path {}", path))?;
    if !ancestor.starts_with(&root) {
        return Err(anyhow!("path escapes sandbox root"));
    }
    Ok(full)
}

fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    while !current.exists() {
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }
    current
}

#[cfg(test)]
mod tests {
    use super::{
        current_host_user_spec, DockerSandbox, DockerSandboxConfig, NoSandbox, ReplaceResult,
        Sandbox, SandboxBashStatus,
    };
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("remi-sandbox-test-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn no_sandbox_fs_and_bash_share_paths() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        sandbox.write("a.txt", b"hello").await.unwrap();
        let out = sandbox.bash("cat a.txt", None, 10_000).await.unwrap();
        assert_eq!(out.stdout, "hello");
        let out = sandbox
            .bash("printf world > b.txt", None, 10_000)
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(sandbox.read("b.txt").await.unwrap(), b"world");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn no_sandbox_accepts_local_absolute_paths() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        let file = root.join("absolute.txt");

        sandbox
            .write(file.to_str().unwrap(), b"absolute")
            .await
            .unwrap();

        assert_eq!(
            sandbox.read(file.to_str().unwrap()).await.unwrap(),
            b"absolute"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn no_sandbox_unnamed_bash_is_stateless() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        let out = sandbox
            .bash("cd /tmp && export REMI_LOCAL_MARK=lost", None, 10_000)
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        let out = sandbox
            .bash("printf \"$PWD:${REMI_LOCAL_MARK:-}\"", None, 10_000)
            .await
            .unwrap();
        assert_eq!(out.stdout, format!("{}:", root.display()));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn no_sandbox_named_bash_preserves_and_isolates_state() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        let out = sandbox
            .bash(
                "cd /tmp && export REMI_LOCAL_MARK=kept",
                Some("alpha"),
                10_000,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        let out = sandbox
            .bash("printf \"$PWD:$REMI_LOCAL_MARK\"", Some("alpha"), 10_000)
            .await
            .unwrap();
        assert_eq!(out.stdout, "/tmp:kept");
        let out = sandbox
            .bash(
                "printf \"${REMI_LOCAL_MARK:-missing}\"",
                Some("beta"),
                10_000,
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, "missing");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn timed_out_unnamed_bash_returns_pid_and_poll_completes() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        let out = sandbox
            .bash("echo start; sleep 0.2; echo done", None, 10)
            .await
            .unwrap();
        assert_eq!(out.status, SandboxBashStatus::Running);
        assert!(out.timed_out);
        let pid = out.pid.clone().expect("timed out bash should return pid");
        let mut combined_stdout = out.stdout;
        let mut completed = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let poll = sandbox.bash_poll(&pid).await.unwrap();
            combined_stdout.push_str(&poll.stdout);
            if poll.status == SandboxBashStatus::Completed {
                completed = Some(poll);
                break;
            }
        }
        let completed = completed.expect("bash task should complete");
        assert_eq!(completed.exit_code, 0);
        assert!(combined_stdout.contains("start"));
        assert!(combined_stdout.contains("done"));

        let repeated = sandbox.bash_poll(&pid).await.unwrap();
        assert_eq!(repeated.status, SandboxBashStatus::Completed);
        assert!(repeated.stdout.is_empty());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn timed_out_unnamed_bash_can_be_cancelled() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        let out = sandbox.bash("sleep 5", None, 10).await.unwrap();
        let pid = out.pid.clone().expect("timed out bash should return pid");

        let cancelled = sandbox.bash_cancel(&pid).await.unwrap();
        assert_eq!(cancelled.status, SandboxBashStatus::Cancelled);
        assert_eq!(cancelled.exit_code, -1);

        let polled = sandbox.bash_poll(&pid).await.unwrap();
        assert_eq!(polled.status, SandboxBashStatus::Cancelled);
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn timed_out_named_bash_blocks_same_name_until_complete() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        let out = sandbox
            .bash(
                "export REMI_LOCAL_MARK=kept; sleep 0.2; printf \"$REMI_LOCAL_MARK\"",
                Some("alpha"),
                10,
            )
            .await
            .unwrap();
        let pid = out
            .pid
            .clone()
            .expect("timed out named bash should return pid");

        let err = sandbox
            .bash("printf should-not-run", Some("alpha"), 10_000)
            .await
            .unwrap_err();
        let err = err.to_string();
        assert!(err.contains("still running task"));
        assert!(err.contains(&pid));

        let beta = sandbox
            .bash("printf beta", Some("beta"), 10_000)
            .await
            .unwrap();
        assert_eq!(beta.stdout, "beta");

        let mut completed = None;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let poll = sandbox.bash_poll(&pid).await.unwrap();
            if poll.status == SandboxBashStatus::Completed {
                completed = Some(poll);
                break;
            }
        }
        let completed = completed.expect("named task should complete");
        assert_eq!(completed.exit_code, 0);

        let after = sandbox
            .bash("printf \"$REMI_LOCAL_MARK\"", Some("alpha"), 10_000)
            .await
            .unwrap();
        assert_eq!(after.stdout, "kept");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn invalid_named_bash_names_are_rejected() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        let err = sandbox
            .bash("true", Some("bad/name"), 10_000)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("named bash session"));
        let too_long = "a".repeat(65);
        let err = sandbox
            .bash("true", Some(&too_long), 10_000)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at most 64"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn no_sandbox_parent_dir_paths_follow_local_shell_semantics() {
        let parent = test_root();
        let root = parent.join("workspace");
        let sandbox = NoSandbox::new(root.clone());

        sandbox.write("../sibling.txt", b"ok").await.unwrap();

        assert_eq!(
            tokio::fs::read(parent.join("sibling.txt")).await.unwrap(),
            b"ok"
        );
        let _ = tokio::fs::remove_dir_all(parent).await;
    }

    #[tokio::test]
    async fn replace_reports_match_counts() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        sandbox.write("note.txt", b"one two one").await.unwrap();
        assert_eq!(
            sandbox.replace("note.txt", "missing", "x").await.unwrap(),
            ReplaceResult::NotFound
        );
        assert_eq!(
            sandbox.replace("note.txt", "one", "x").await.unwrap(),
            ReplaceResult::MultipleMatches(2)
        );
        assert_eq!(
            sandbox.replace("note.txt", "two", "2").await.unwrap(),
            ReplaceResult::Replaced
        );
        assert_eq!(sandbox.read("note.txt").await.unwrap(), b"one 2 one");
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn docker_sandbox_accepts_workspace_absolute_paths_for_fs_tools() {
        let root = test_root();
        let sandbox = DockerSandbox::new(DockerSandboxConfig {
            host_dir: root.clone(),
            container_dir: "/workspace".to_string(),
            image: "unused".to_string(),
            container_name: "unused".to_string(),
            user: None,
        });

        sandbox
            .mkdir("/workspace/dir", true)
            .await
            .expect("workspace absolute mkdir should succeed");
        sandbox
            .write("/workspace/dir/note.txt", b"hello")
            .await
            .expect("workspace absolute write should succeed");
        assert_eq!(
            sandbox.read("/workspace/dir/note.txt").await.unwrap(),
            b"hello"
        );
        assert_eq!(
            sandbox
                .replace("/workspace/dir/note.txt", "hello", "world")
                .await
                .unwrap(),
            ReplaceResult::Replaced
        );
        assert_eq!(
            sandbox.list("/workspace/dir").await.unwrap(),
            vec!["note.txt".to_string()]
        );
        sandbox
            .remove("/workspace/dir/note.txt", false)
            .await
            .unwrap();
        assert!(!root.join("dir/note.txt").exists());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn docker_sandbox_rejects_host_absolute_paths() {
        let root = test_root();
        let sandbox = DockerSandbox::new(DockerSandboxConfig {
            host_dir: root.clone(),
            container_dir: "/workspace".to_string(),
            image: "unused".to_string(),
            container_name: "unused".to_string(),
            user: None,
        });
        let host_path = root.join("note.txt");

        let err = sandbox.write(host_path.to_str().unwrap(), b"nope").await;
        assert!(err
            .unwrap_err()
            .to_string()
            .contains("absolute sandbox path must start with /workspace"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    #[ignore = "requires Docker and a local bash-capable image"]
    async fn docker_sandbox_fs_and_bash_share_paths() {
        let root = test_root();
        let container_name = format!("remi-sandbox-test-{}", uuid::Uuid::new_v4());
        let image = std::env::var("REMI_DOCKER_TEST_IMAGE")
            .unwrap_or_else(|_| "mcr.microsoft.com/devcontainers/base:bookworm".to_string());
        let sandbox = DockerSandbox::new(DockerSandboxConfig {
            host_dir: root.clone(),
            container_dir: "/workspace".to_string(),
            image,
            container_name: container_name.clone(),
            user: current_host_user_spec(),
        });

        sandbox.write("from_fs.txt", b"from-fs").await.unwrap();
        let out = sandbox.bash("cat from_fs.txt", None, 30_000).await.unwrap();
        assert_eq!(out.stdout, "from-fs");
        let out = sandbox
            .bash("printf from-bash > from_bash.txt", None, 30_000)
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(sandbox.read("from_bash.txt").await.unwrap(), b"from-bash");

        let out = sandbox
            .bash(
                "cd /tmp && export REMI_SESSION_MARK=kept",
                Some("named"),
                30_000,
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        let out = sandbox
            .bash("printf \"$PWD:$REMI_SESSION_MARK\"", Some("named"), 30_000)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "/tmp:kept");

        let _ = Command::new("docker")
            .args(["rm", "-f", &container_name])
            .output();
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
