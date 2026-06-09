use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

pub type SandboxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxBashOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
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
}

impl NoSandbox {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    async fn bash_once(&self, command: &str, timeout_ms: u64) -> Result<SandboxBashOutput> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .context("creating sandbox root")?;
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&self.root);
        run_command_with_timeout(cmd, timeout_ms).await
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
            let full = resolve_existing_path(&self.root, path).await?;
            tokio::fs::read(full).await.context("reading sandbox file")
        })
    }

    fn write<'a>(&'a self, path: &'a str, content: &'a [u8]) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let full = resolve_writable_file_path(&self.root, path).await?;
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
            let full = resolve_existing_path(&self.root, path).await?;
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
            let full = resolve_creatable_dir_path(&self.root, path).await?;
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
            let full = resolve_existing_path(&self.root, path).await?;
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
            let full = resolve_existing_path(&self.root, path).await?;
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
                Some(named) => self.bash_named(named, command, timeout_ms).await,
                None => self.bash_once(command, timeout_ms).await,
            }
        })
    }
}

#[derive(Clone)]
pub struct DockerSandbox {
    config: DockerSandboxConfig,
    sessions: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<BashSession>>>>>,
}

impl DockerSandbox {
    pub fn new(config: DockerSandboxConfig) -> Self {
        Self {
            config,
            sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
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
        cmd.args([&self.config.container_name, "bash", "-lc", command]);
        run_command_with_timeout(cmd, timeout_ms).await
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
            let full = resolve_existing_path(&self.config.host_dir, path).await?;
            tokio::fs::read(full).await.context("reading sandbox file")
        })
    }

    fn write<'a>(&'a self, path: &'a str, content: &'a [u8]) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let full = resolve_writable_file_path(&self.config.host_dir, path).await?;
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
            let local = NoSandbox::new(self.config.host_dir.clone());
            local.replace(path, old, new).await
        })
    }

    fn mkdir<'a>(&'a self, path: &'a str, recursive: bool) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let local = NoSandbox::new(self.config.host_dir.clone());
            local.mkdir(path, recursive).await
        })
    }

    fn remove<'a>(&'a self, path: &'a str, recursive: bool) -> SandboxFuture<'a, ()> {
        Box::pin(async move {
            let local = NoSandbox::new(self.config.host_dir.clone());
            local.remove(path, recursive).await
        })
    }

    fn list<'a>(&'a self, path: &'a str) -> SandboxFuture<'a, Vec<String>> {
        Box::pin(async move {
            let local = NoSandbox::new(self.config.host_dir.clone());
            local.list(path).await
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
}

#[derive(Debug)]
struct BashSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl BashSession {
    async fn start_local(root: &Path) -> Result<Self> {
        let child = Command::new("bash")
            .args(["--noprofile", "--norc"])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting named bash session")?;
        Self::from_child(child, "named bash session")
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
                "-lc",
                "exec 2>&1; exec bash --noprofile --norc",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting named bash session")?;
        Self::from_child(child, "named bash session")
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

    async fn run(&mut self, command: &str) -> Result<SandboxBashOutput> {
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

        let mut stdout = String::new();
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
                if stdout.ends_with('\n') {
                    stdout.pop();
                    if stdout.ends_with('\r') {
                        stdout.pop();
                    }
                }
                return Ok(SandboxBashOutput {
                    stdout,
                    stderr: String::new(),
                    exit_code,
                    timed_out: false,
                });
            }
            stdout.push_str(&line);
        }
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

async fn run_command_with_timeout(mut cmd: Command, timeout_ms: u64) -> Result<SandboxBashOutput> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let run = cmd.output();
    match tokio::time::timeout(Duration::from_millis(timeout_ms), run).await {
        Err(_) => Ok(SandboxBashOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
            timed_out: true,
        }),
        Ok(Err(err)) => Err(err).context("running sandbox command"),
        Ok(Ok(output)) => Ok(SandboxBashOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
            timed_out: false,
        }),
    }
}

async fn run_named_bash<F, Fut>(
    sessions: &Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<BashSession>>>>>,
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

    let run = async {
        let mut session = session.lock().await;
        session.run(command).await
    };
    match tokio::time::timeout(Duration::from_millis(timeout_ms), run).await {
        Ok(result) => result,
        Err(_) => {
            let mut sessions = sessions.lock().await;
            if let Some(session) = sessions.remove(&named) {
                let mut session = session.lock().await;
                session.kill().await;
            }
            Ok(SandboxBashOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: -1,
                timed_out: true,
            })
        }
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

fn clean_relative_path(path: &str) -> Result<PathBuf> {
    let path = Path::new(path);
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

async fn resolve_existing_path(root: &Path, path: &str) -> Result<PathBuf> {
    let root = canonical_root(root).await?;
    let relative = clean_relative_path(path)?;
    let full = root.join(relative);
    let canonical = tokio::fs::canonicalize(&full)
        .await
        .with_context(|| format!("canonicalizing sandbox path {}", path))?;
    if !canonical.starts_with(&root) {
        return Err(anyhow!("path escapes sandbox root"));
    }
    Ok(canonical)
}

async fn resolve_writable_file_path(root: &Path, path: &str) -> Result<PathBuf> {
    let root = canonical_root(root).await?;
    let relative = clean_relative_path(path)?;
    if relative == Path::new(".") {
        return Err(anyhow!("path must refer to a file"));
    }
    let full = root.join(relative);
    if tokio::fs::symlink_metadata(&full).await.is_ok() {
        let canonical = tokio::fs::canonicalize(&full)
            .await
            .with_context(|| format!("canonicalizing sandbox path {}", path))?;
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
        .with_context(|| format!("canonicalizing parent for sandbox path {}", path))?;
    if !parent.starts_with(&root) {
        return Err(anyhow!("path escapes sandbox root"));
    }
    Ok(full)
}

async fn resolve_creatable_dir_path(root: &Path, path: &str) -> Result<PathBuf> {
    let root = canonical_root(root).await?;
    let relative = clean_relative_path(path)?;
    let full = root.join(relative);
    if tokio::fs::symlink_metadata(&full).await.is_ok() {
        let canonical = tokio::fs::canonicalize(&full)
            .await
            .with_context(|| format!("canonicalizing sandbox path {}", path))?;
        if !canonical.starts_with(&root) {
            return Err(anyhow!("path escapes sandbox root"));
        }
        return Ok(canonical);
    }
    let mut probe = full.as_path();
    while !probe.exists() {
        probe = probe
            .parent()
            .ok_or_else(|| anyhow!("path has no existing parent"))?;
    }
    let canonical = tokio::fs::canonicalize(probe)
        .await
        .with_context(|| format!("canonicalizing sandbox path {}", path))?;
    if !canonical.starts_with(&root) {
        return Err(anyhow!("path escapes sandbox root"));
    }
    Ok(full)
}

#[cfg(test)]
mod tests {
    use super::{
        current_host_user_spec, DockerSandbox, DockerSandboxConfig, NoSandbox, ReplaceResult,
        Sandbox,
    };
    use std::path::PathBuf;
    use std::process::Command;

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
    async fn rejects_parent_dir_escape() {
        let root = test_root();
        let sandbox = NoSandbox::new(root.clone());
        let err = sandbox.write("../outside.txt", b"nope").await.unwrap_err();
        assert!(err.to_string().contains(".."));
        let _ = tokio::fs::remove_dir_all(root).await;
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
