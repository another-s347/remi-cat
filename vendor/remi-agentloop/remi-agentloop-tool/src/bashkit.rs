use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use futures::Stream;
use tokio::sync::Mutex;

use remi_core::error::AgentError;
use remi_core::tool::{Tool, ToolContext, ToolOutput, ToolResult};
use remi_core::types::ResumePayload;

// ── LocalFsBackend ────────────────────────────────────────────────────────────

/// A [`bashkit::FsBackend`] that proxies to the real host filesystem under a
/// chroot-like root directory. All paths presented to bashkit scripts are
/// treated as relative to `root`; the host filesystem is never accessed
/// outside that subtree.
struct LocalFsBackend {
    root: PathBuf,
}

impl LocalFsBackend {
    fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Translate a virtual absolute path (e.g. `/data/file.txt`) to a real
    /// path by joining it under `self.root`.  Returns an error if the resolved
    /// path would escape the root (path traversal via `..`).
    fn resolve(&self, path: &Path) -> std::io::Result<PathBuf> {
        let stripped = path.strip_prefix("/").unwrap_or(path);
        let candidate = self.root.join(stripped);
        let clean = lexical_clean(&candidate);
        let clean_root = lexical_clean(&self.root);
        if !clean.starts_with(&clean_root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "path traversal outside sandbox root",
            ));
        }
        Ok(candidate)
    }
}

/// Lexically resolve `.` and `..` components without touching the filesystem.
fn lexical_clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[async_trait]
impl bashkit::FsBackend for LocalFsBackend {
    async fn read(&self, path: &Path) -> bashkit::Result<Vec<u8>> {
        let p = self.resolve(path).map_err(bashkit::Error::Io)?;
        tokio::fs::read(&p).await.map_err(bashkit::Error::Io)
    }

    async fn write(&self, path: &Path, content: &[u8]) -> bashkit::Result<()> {
        let p = self.resolve(path).map_err(bashkit::Error::Io)?;
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(bashkit::Error::Io)?;
        }
        tokio::fs::write(&p, content)
            .await
            .map_err(bashkit::Error::Io)
    }

    async fn append(&self, path: &Path, content: &[u8]) -> bashkit::Result<()> {
        use tokio::io::AsyncWriteExt as _;
        let p = self.resolve(path).map_err(bashkit::Error::Io)?;
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(bashkit::Error::Io)?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .await
            .map_err(bashkit::Error::Io)?;
        file.write_all(content).await.map_err(bashkit::Error::Io)
    }

    async fn mkdir(&self, path: &Path, recursive: bool) -> bashkit::Result<()> {
        let p = self.resolve(path).map_err(bashkit::Error::Io)?;
        if recursive {
            tokio::fs::create_dir_all(&p)
                .await
                .map_err(bashkit::Error::Io)
        } else {
            tokio::fs::create_dir(&p).await.map_err(bashkit::Error::Io)
        }
    }

    async fn remove(&self, path: &Path, recursive: bool) -> bashkit::Result<()> {
        let p = self.resolve(path).map_err(bashkit::Error::Io)?;
        let meta = tokio::fs::metadata(&p).await.map_err(bashkit::Error::Io)?;
        if meta.is_dir() {
            if recursive {
                tokio::fs::remove_dir_all(&p)
                    .await
                    .map_err(bashkit::Error::Io)
            } else {
                tokio::fs::remove_dir(&p).await.map_err(bashkit::Error::Io)
            }
        } else {
            tokio::fs::remove_file(&p).await.map_err(bashkit::Error::Io)
        }
    }

    async fn stat(&self, path: &Path) -> bashkit::Result<bashkit::Metadata> {
        let p = self.resolve(path).map_err(bashkit::Error::Io)?;
        let meta = tokio::fs::symlink_metadata(&p)
            .await
            .map_err(bashkit::Error::Io)?;
        let file_type = if meta.is_dir() {
            bashkit::FileType::Directory
        } else if meta.is_symlink() {
            bashkit::FileType::Symlink
        } else {
            bashkit::FileType::File
        };
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::MetadataExt as _;
            meta.mode() & 0o7777
        };
        #[cfg(not(unix))]
        let mode = if meta.is_dir() { 0o755 } else { 0o644 };
        Ok(bashkit::Metadata {
            file_type,
            size: meta.len(),
            mode,
            modified: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            created: meta.created().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
        })
    }

    async fn read_dir(&self, path: &Path) -> bashkit::Result<Vec<bashkit::DirEntry>> {
        let p = self.resolve(path).map_err(bashkit::Error::Io)?;
        let mut reader = tokio::fs::read_dir(&p).await.map_err(bashkit::Error::Io)?;
        let mut entries = Vec::new();
        loop {
            match reader.next_entry().await.map_err(bashkit::Error::Io)? {
                None => break,
                Some(entry) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let meta = entry.metadata().await.map_err(bashkit::Error::Io)?;
                    let file_type = if meta.is_dir() {
                        bashkit::FileType::Directory
                    } else if meta.is_symlink() {
                        bashkit::FileType::Symlink
                    } else {
                        bashkit::FileType::File
                    };
                    #[cfg(unix)]
                    let mode = {
                        use std::os::unix::fs::MetadataExt as _;
                        meta.mode() & 0o7777
                    };
                    #[cfg(not(unix))]
                    let mode = if meta.is_dir() { 0o755 } else { 0o644 };
                    entries.push(bashkit::DirEntry {
                        name,
                        metadata: bashkit::Metadata {
                            file_type,
                            size: meta.len(),
                            mode,
                            modified: meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                            created: meta.created().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                        },
                    });
                }
            }
        }
        Ok(entries)
    }

    async fn exists(&self, path: &Path) -> bashkit::Result<bool> {
        let p = self.resolve(path).map_err(bashkit::Error::Io)?;
        Ok(tokio::fs::try_exists(&p).await.unwrap_or(false))
    }

    async fn rename(&self, from: &Path, to: &Path) -> bashkit::Result<()> {
        let f = self.resolve(from).map_err(bashkit::Error::Io)?;
        let t = self.resolve(to).map_err(bashkit::Error::Io)?;
        tokio::fs::rename(&f, &t).await.map_err(bashkit::Error::Io)
    }

    async fn copy(&self, from: &Path, to: &Path) -> bashkit::Result<()> {
        let f = self.resolve(from).map_err(bashkit::Error::Io)?;
        let t = self.resolve(to).map_err(bashkit::Error::Io)?;
        tokio::fs::copy(&f, &t)
            .await
            .map(|_| ())
            .map_err(bashkit::Error::Io)
    }

    async fn symlink(&self, target: &Path, link: &Path) -> bashkit::Result<()> {
        #[cfg(unix)]
        {
            let t = self.resolve(target).map_err(bashkit::Error::Io)?;
            let l = self.resolve(link).map_err(bashkit::Error::Io)?;
            tokio::fs::symlink(&t, &l).await.map_err(bashkit::Error::Io)
        }
        #[cfg(not(unix))]
        {
            let _ = (target, link);
            Err(bashkit::Error::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "symlinks not supported on this platform",
            )))
        }
    }

    async fn read_link(&self, path: &Path) -> bashkit::Result<PathBuf> {
        let p = self.resolve(path).map_err(bashkit::Error::Io)?;
        tokio::fs::read_link(&p).await.map_err(bashkit::Error::Io)
    }

    async fn chmod(&self, path: &Path, mode: u32) -> bashkit::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let p = self.resolve(path).map_err(bashkit::Error::Io)?;
            let perms = std::fs::Permissions::from_mode(mode);
            tokio::fs::set_permissions(&p, perms)
                .await
                .map_err(bashkit::Error::Io)
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
            Ok(()) // no-op on Windows
        }
    }
}

// ── MountSource / Mount ──────────────────────────────────────────────────────

/// The backing source for a single mount point in [`FsMode::Mixed`].
///
/// A mount can be backed by either a host directory (chrooted through
/// [`LocalFsBackend`]) or any arbitrary [`bashkit::FileSystem`] implementation,
/// enabling business-domain objects (databases, object stores, APIs, …) to be
/// surfaced as files inside the virtual bash environment.
#[derive(Clone)]
pub enum MountSource {
    /// Back the mount point with a host directory, chrooted for safety.
    ///
    /// The directory is created automatically if it does not exist.
    Host {
        /// Absolute path on the real filesystem to use as the mount root.
        host_path: PathBuf,
    },

    /// Back the mount point with an arbitrary [`bashkit::FileSystem`].
    ///
    /// This variant lets you mount any custom in-memory, database-backed,
    /// or network-backed filesystem without going through the real host FS.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use remi_tool::bashkit::{MountSource, Mount, FsMode, VirtualBashTool};
    /// use std::sync::Arc;
    ///
    /// // An in-memory fs pre-populated with data
    /// let mem = bashkit::InMemoryFs::new();
    /// // … populate mem …
    ///
    /// let tool = VirtualBashTool::builder()
    ///     .fs_mode(FsMode::Mixed {
    ///         mounts: vec![Mount {
    ///             virtual_path: "/db".into(),
    ///             source: MountSource::Custom(Arc::new(mem)),
    ///         }],
    ///     })
    ///     .build()
    ///     .unwrap();
    /// ```
    Custom(Arc<dyn bashkit::FileSystem>),
}

impl std::fmt::Debug for MountSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountSource::Host { host_path } => f
                .debug_struct("Host")
                .field("host_path", host_path)
                .finish(),
            MountSource::Custom(_) => write!(f, "Custom(Arc<dyn FileSystem>)"),
        }
    }
}

/// A single mount entry for [`FsMode::Mixed`].
///
/// Pairs a virtual path inside the bash environment with a [`MountSource`]
/// that provides the backing filesystem for that subtree.
#[derive(Clone, Debug)]
pub struct Mount {
    /// Absolute virtual path at which to mount the source (e.g. `"/workspace"`).
    pub virtual_path: String,
    /// The filesystem source backing this mount point.
    pub source: MountSource,
}

impl Mount {
    /// Convenience constructor.
    pub fn new(virtual_path: impl Into<String>, source: MountSource) -> Self {
        Self {
            virtual_path: virtual_path.into(),
            source,
        }
    }

    /// Shorthand for a host-backed mount.
    pub fn host(virtual_path: impl Into<String>, host_path: impl Into<PathBuf>) -> Self {
        Self::new(
            virtual_path,
            MountSource::Host {
                host_path: host_path.into(),
            },
        )
    }

    /// Shorthand for a custom-filesystem mount.
    pub fn custom(virtual_path: impl Into<String>, fs: Arc<dyn bashkit::FileSystem>) -> Self {
        Self::new(virtual_path, MountSource::Custom(fs))
    }
}

// ── FsMode ────────────────────────────────────────────────────────────────────

/// Filesystem configuration for [`VirtualBashTool`].
///
/// Controls whether script execution uses a purely virtual in-memory
/// filesystem, the real host filesystem (sandboxed under a root directory),
/// or a hybrid where specific directories from the host are mounted into
/// an otherwise virtual tree.
#[derive(Clone, Debug)]
pub enum FsMode {
    /// **Pure virtual** (default): all file I/O is fully sandboxed in memory.
    ///
    /// Nothing is written to or read from the real host filesystem.
    /// Ideal for untrusted scripts.
    Virtual,

    /// **Local**: the virtual filesystem root maps directly to a host directory.
    ///
    /// `root` is a host path that acts as `/` inside the bash environment.
    /// Every file access is relative to `root`; attempts to escape via `..`
    /// are rejected. The directory is created automatically if it does not exist.
    Local {
        /// Host directory to use as the virtual `/`.
        root: PathBuf,
    },

    /// **Mixed**: virtual in-memory filesystem as the base, with one or more
    /// mount points backed by either host directories or custom filesystems.
    ///
    /// Scripts see a normal virtual tree everywhere except the mount points.
    ///
    /// ```text
    /// virtual /          ← InMemoryFs        (all unmatched paths)
    /// virtual /workspace ← host: ~/project   (Mount::host)
    /// virtual /db        ← Arc<MyDbBackend>   (Mount::custom)
    /// ```
    Mixed {
        /// Ordered list of mount entries.
        ///
        /// Each [`Mount`] pairs an absolute virtual path (e.g. `"/workspace"`)
        /// with a [`MountSource`] (`Host` or `Custom`). Host directories are
        /// created automatically if they do not exist. Custom filesystems are
        /// mounted as-is without any initialization.
        mounts: Vec<Mount>,
    },
}

impl Default for FsMode {
    fn default() -> Self {
        FsMode::Virtual
    }
}

impl FsMode {
    /// Build an `Arc<dyn FileSystem>` from this mode.
    ///
    /// Returns an I/O error if host directories cannot be created.
    ///
    /// Pass the result to [`crate::bkfs::FsToolkit::new`] to create FS tools
    /// backed by this mode, or to [`VirtualBashToolBuilder::build`] via the
    /// builder's `.fs_mode()` setter.
    pub fn build_fs(self) -> std::io::Result<Arc<dyn bashkit::FileSystem>> {
        match self {
            FsMode::Virtual => Ok(Arc::new(bashkit::InMemoryFs::new())),

            FsMode::Local { root } => {
                std::fs::create_dir_all(&root)?;
                Ok(Arc::new(bashkit::PosixFs::new(LocalFsBackend::new(root))))
            }

            FsMode::Mixed { mounts } => {
                let base: Arc<dyn bashkit::FileSystem> = Arc::new(bashkit::InMemoryFs::new());
                let mountable = bashkit::MountableFs::new(base);
                for mount in mounts {
                    let fs: Arc<dyn bashkit::FileSystem> = match mount.source {
                        MountSource::Host { host_path } => {
                            std::fs::create_dir_all(&host_path)?;
                            Arc::new(bashkit::PosixFs::new(LocalFsBackend::new(host_path)))
                        }
                        MountSource::Custom(fs) => fs,
                    };
                    mountable
                        .mount(&mount.virtual_path, fs)
                        .map_err(|e| std::io::Error::other(e.to_string()))?;
                }
                Ok(Arc::new(mountable))
            }
        }
    }
}

// ── VirtualBashTool ───────────────────────────────────────────────────────────

/// Virtual bash interpreter tool powered by [bashkit](https://docs.rs/bashkit).
///
/// Executes bash commands in a sandboxed, in-process virtual environment with:
/// - **Configurable filesystem** — pure virtual, local, or mixed (see [`FsMode`])
/// - **Resource limits** — command count, loop iterations, function depth, timeout
/// - **Network allowlist** — controlled HTTP access (curl/wget)
/// - **100+ built-in commands** — echo, grep, sed, awk, jq, tar, etc.
/// - **Cross-platform** — works on Linux, macOS, and Windows (no system bash needed)
///
/// The interpreter is stateful: variables, virtual files, and shell state persist
/// across multiple `execute()` calls within the same `VirtualBashTool` instance.
///
/// # Filesystem Modes
///
/// | Mode | Description |
/// |------|-------------|
/// | [`FsMode::Virtual`] | Pure in-memory sandbox (default) |
/// | [`FsMode::Local`] | Real host FS rooted at a directory |
/// | [`FsMode::Mixed`] | Virtual + host dirs mounted at specific paths |
///
/// # Example
///
/// ```rust,no_run
/// use remi_tool::bashkit::{VirtualBashTool, FsMode};
/// use std::path::PathBuf;
///
/// // Default: pure virtual
/// let tool = VirtualBashTool::new();
///
/// // Local FS rooted at /tmp/sandbox
/// let tool = VirtualBashTool::builder()
///     .fs_mode(FsMode::Local { root: PathBuf::from("/tmp/sandbox") })
///     .build()
///     .unwrap();
///
/// // Mixed: virtual base + /workspace mounted from host
/// let tool = VirtualBashTool::builder()
///     .fs_mode(FsMode::Mixed {
///         mounts: vec![Mount::host("/workspace", "/home/user/project")],
///     })
///     .build()
///     .unwrap();
/// ```
pub struct VirtualBashTool {
    inner: Arc<Mutex<bashkit::Bash>>,
    name: String,
}

impl VirtualBashTool {
    /// Create a new `VirtualBashTool` with default configuration (pure virtual filesystem).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(bashkit::Bash::new())),
            name: "bash".into(),
        }
    }

    /// Create a builder for advanced configuration.
    pub fn builder() -> VirtualBashToolBuilder {
        VirtualBashToolBuilder::new()
    }
}

impl Default for VirtualBashTool {
    fn default() -> Self {
        Self::new()
    }
}

// ── VirtualBashToolBuilder ────────────────────────────────────────────────────

/// Builder for configuring a [`VirtualBashTool`].
///
/// # Example
///
/// ```rust,no_run
/// use remi_tool::bashkit::{VirtualBashTool, FsMode};
/// use std::path::PathBuf;
///
/// let tool = VirtualBashTool::builder()
///     .name("shell")
///     .username("deploy")
///     .hostname("prod-server")
///     .env("HOME", "/home/deploy")
///     .fs_mode(FsMode::Mixed {
///         mounts: vec![
///             Mount::host("/workspace", "/home/deploy/app"),
///             // Mount::custom("/db", Arc::new(MyDbFs::new(pool))),
///         ],
///     })
///     .build()
///     .unwrap();
/// ```
pub struct VirtualBashToolBuilder {
    bash_builder: bashkit::BashBuilder,
    fs_mode: FsMode,
    name: String,
}

impl VirtualBashToolBuilder {
    fn new() -> Self {
        Self {
            bash_builder: bashkit::Bash::builder(),
            fs_mode: FsMode::Virtual,
            name: "bash".into(),
        }
    }

    /// Override the tool name exposed to the LLM (default: `"bash"`).
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Set the filesystem mode.
    ///
    /// See [`FsMode`] for the three available modes:
    /// - [`FsMode::Virtual`] — in-memory sandbox (default)
    /// - [`FsMode::Local`] — real host FS under a root directory
    /// - [`FsMode::Mixed`] — virtual base with host mounts
    pub fn fs_mode(mut self, mode: FsMode) -> Self {
        self.fs_mode = mode;
        self
    }

    /// Set custom username for virtual identity (`whoami`, `$USER`).
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.bash_builder = self.bash_builder.username(username);
        self
    }

    /// Set custom hostname for virtual identity (`hostname`, `uname -n`).
    pub fn hostname(mut self, hostname: impl Into<String>) -> Self {
        self.bash_builder = self.bash_builder.hostname(hostname);
        self
    }

    /// Set execution limits (command count, loop iterations, timeout, etc.).
    pub fn limits(mut self, limits: bashkit::ExecutionLimits) -> Self {
        self.bash_builder = self.bash_builder.limits(limits);
        self
    }

    /// Add an environment variable visible to scripts.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.bash_builder = self.bash_builder.env(key, value);
        self
    }

    /// Set the initial working directory inside the virtual environment.
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.bash_builder = self.bash_builder.cwd(cwd);
        self
    }

    /// Build the configured [`VirtualBashTool`].
    ///
    /// # Errors
    ///
    /// Returns an `std::io::Error` if host directories required by
    /// [`FsMode::Local`] or [`FsMode::Mixed`] cannot be created.
    pub fn build(self) -> std::io::Result<VirtualBashTool> {
        let fs = self.fs_mode.build_fs()?;
        let bash = self.bash_builder.fs(fs).build();
        Ok(VirtualBashTool {
            inner: Arc::new(Mutex::new(bash)),
            name: self.name,
        })
    }
}

// ── Tool impl ─────────────────────────────────────────────────────────────────

impl Tool for VirtualBashTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Execute bash commands in a sandboxed virtual environment. \
         Supports variables, pipes, redirections, control flow, \
         functions, and 100+ built-in commands (grep, sed, awk, jq, etc.). \
         File operations use the configured filesystem (virtual, local, or mixed)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command(s) to execute"
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
        let inner = self.inner.clone();

        async move {
            let command = arguments["command"]
                .as_str()
                .ok_or_else(|| AgentError::tool("bash", "missing 'command' argument"))?
                .to_string();

            let mut bash = inner.lock().await;
            let result = bash.exec(&command).await;
            drop(bash);

            let (exit_code, stdout, stderr) = match result {
                Ok(r) => (r.exit_code, r.stdout, r.stderr),
                Err(e) => (1, String::new(), e.to_string()),
            };

            let cmd_display = command.clone();
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::Delta(format!("$ {}", cmd_display));

                let result = if !stderr.is_empty() {
                    serde_json::json!({
                        "exit_code": exit_code,
                        "stdout": stdout,
                        "stderr": stderr,
                    })
                    .to_string()
                } else {
                    serde_json::json!({
                        "exit_code": exit_code,
                        "stdout": stdout,
                    })
                    .to_string()
                };

                yield ToolOutput::text(result);
            }))
        }
    }
}
