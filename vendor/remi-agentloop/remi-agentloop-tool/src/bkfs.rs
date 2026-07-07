//! Filesystem tools backed by [`bashkit::FileSystem`].
//!
//! Five focused, separate tools — each wrapping a shared `Arc<dyn
//! bashkit::FileSystem>` — that expose read, write, mkdir, remove, and ls
//! operations to the LLM.  All tools see the **same** filesystem state
//! because they share a single `Arc`.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use remi_tool::bkfs::FsToolkit;
//! use std::sync::Arc;
//!
//! // Pure in-memory virtual filesystem
//! let kit = FsToolkit::new(Arc::new(bashkit::InMemoryFs::new()));
//!
//! // Register all five tools with an agent builder
//! // builder.tool(kit.read()).tool(kit.write()).tool(kit.mkdir())
//! //        .tool(kit.remove()).tool(kit.ls());
//! ```
//!
//! If you have a [`crate::bashkit::FsMode`] (requires `tool-bash-virtual`),
//! build the filesystem first:
//! ```rust,no_run
//! let kit = FsToolkit::new(FsMode::Virtual.build_fs().unwrap());
//! ```

use std::path::Path;
use std::sync::Arc;

use async_stream::stream;
use futures::Stream;

use remi_core::error::AgentError;
use remi_core::tool::{Tool, ToolContext, ToolOutput, ToolResult};
use remi_core::types::ResumePayload;

// ── internal helpers ──────────────────────────────────────────────────────────

fn map_err(tool: &'static str, e: bashkit::Error) -> AgentError {
    AgentError::tool(tool, e.to_string())
}

fn need_path(args: &serde_json::Value, tool: &'static str) -> Result<String, AgentError> {
    args["path"]
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| AgentError::tool(tool, "missing required argument 'path'"))
}

// ── FsReadTool ────────────────────────────────────────────────────────────────

/// Reads the entire content of a file from the shared virtual filesystem.
///
/// Binary content is decoded to UTF-8 with lossless replacement of invalid
/// bytes.
#[derive(Clone)]
pub struct FsReadTool {
    fs: Arc<dyn bashkit::FileSystem>,
}

impl FsReadTool {
    /// Create wrapping the given filesystem.
    pub fn new(fs: Arc<dyn bashkit::FileSystem>) -> Self {
        Self { fs }
    }
}

impl Tool for FsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }

    fn description(&self) -> &str {
        "Read the full contents of a file in the virtual filesystem and return \
         it as a UTF-8 string."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path of the file to read, e.g. \"/data/config.json\""
                }
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
        let fs = self.fs.clone();
        async move {
            let path_str = need_path(&arguments, "fs_read")?;
            let bytes = fs
                .read_file(Path::new(&path_str))
                .await
                .map_err(|e| map_err("fs_read", e))?;
            let content = String::from_utf8_lossy(&bytes).into_owned();
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::text(content);
            }))
        }
    }
}

// ── FsWriteTool ───────────────────────────────────────────────────────────────

/// Writes (or overwrites) a file in the shared virtual filesystem.
///
/// Creates the file if it does not exist.  The parent directory must already
/// exist; use [`FsCreateTool`] (mkdir) to create it first.
#[derive(Clone)]
pub struct FsWriteTool {
    fs: Arc<dyn bashkit::FileSystem>,
}

impl FsWriteTool {
    pub fn new(fs: Arc<dyn bashkit::FileSystem>) -> Self {
        Self { fs }
    }
}

impl Tool for FsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }

    fn description(&self) -> &str {
        "Write text content to a file in the virtual filesystem, creating the file \
         if it does not exist or overwriting it if it does. \
         The parent directory must exist (use fs_mkdir first)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path of the file to write, e.g. \"/workspace/result.txt\""
                },
                "content": {
                    "type": "string",
                    "description": "Text content to write to the file"
                }
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
        let fs = self.fs.clone();
        async move {
            let path_str = need_path(&arguments, "fs_write")?;
            let content = arguments["content"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_write", "missing required argument 'content'"))?
                .to_owned();
            let bytes = content.len();
            fs.write_file(Path::new(&path_str), content.as_bytes())
                .await
                .map_err(|e| map_err("fs_write", e))?;
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::text(format!("wrote {} bytes to {}", bytes, path_str));
            }))
        }
    }
}

// ── FsCreateTool ──────────────────────────────────────────────────────────────

/// Creates a directory in the shared virtual filesystem.
///
/// Set `recursive = true` for `mkdir -p` behaviour (create all missing
/// parent directories).
#[derive(Clone)]
pub struct FsCreateTool {
    fs: Arc<dyn bashkit::FileSystem>,
}

impl FsCreateTool {
    pub fn new(fs: Arc<dyn bashkit::FileSystem>) -> Self {
        Self { fs }
    }
}

impl Tool for FsCreateTool {
    fn name(&self) -> &str {
        "fs_mkdir"
    }

    fn description(&self) -> &str {
        "Create a directory (and, when recursive=true, all missing parent \
         directories) in the virtual filesystem."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path of the directory to create, e.g. \"/workspace/output\""
                },
                "recursive": {
                    "type": "boolean",
                    "description": "Create parent directories as needed (like mkdir -p). Defaults to false.",
                    "default": false
                }
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
        let fs = self.fs.clone();
        async move {
            let path_str = need_path(&arguments, "fs_mkdir")?;
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            fs.mkdir(Path::new(&path_str), recursive)
                .await
                .map_err(|e| map_err("fs_mkdir", e))?;
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::text(format!("created directory {}", path_str));
            }))
        }
    }
}

// ── FsRemoveTool ──────────────────────────────────────────────────────────────

/// Removes a file or directory from the shared virtual filesystem.
///
/// Directories must be empty unless `recursive = true`.
#[derive(Clone)]
pub struct FsRemoveTool {
    fs: Arc<dyn bashkit::FileSystem>,
}

impl FsRemoveTool {
    pub fn new(fs: Arc<dyn bashkit::FileSystem>) -> Self {
        Self { fs }
    }
}

impl Tool for FsRemoveTool {
    fn name(&self) -> &str {
        "fs_remove"
    }

    fn description(&self) -> &str {
        "Remove a file or directory from the virtual filesystem. \
         Set recursive=true to remove a non-empty directory and all its contents."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path of the file or directory to remove"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "If true, remove directory contents recursively (like rm -r). Defaults to false.",
                    "default": false
                }
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
        let fs = self.fs.clone();
        async move {
            let path_str = need_path(&arguments, "fs_remove")?;
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            fs.remove(Path::new(&path_str), recursive)
                .await
                .map_err(|e| map_err("fs_remove", e))?;
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::text(format!("removed {}", path_str));
            }))
        }
    }
}

// ── FsLsTool ──────────────────────────────────────────────────────────────────

/// Lists the contents of a directory in the shared virtual filesystem.
///
/// Returns a JSON array where each element is an object with:
/// - `name` — entry name (not the full path)
/// - `type` — `"file"`, `"directory"`, or `"symlink"`
/// - `size` — size in bytes
#[derive(Clone)]
pub struct FsLsTool {
    fs: Arc<dyn bashkit::FileSystem>,
}

impl FsLsTool {
    pub fn new(fs: Arc<dyn bashkit::FileSystem>) -> Self {
        Self { fs }
    }
}

impl Tool for FsLsTool {
    fn name(&self) -> &str {
        "fs_ls"
    }

    fn description(&self) -> &str {
        "List the contents of a directory in the virtual filesystem. \
         Returns a JSON array of entries, each with name, type \
         (file/directory/symlink), and size in bytes."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path of the directory to list, e.g. \"/\" or \"/workspace\""
                }
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
        let fs = self.fs.clone();
        async move {
            let path_str = need_path(&arguments, "fs_ls")?;
            let entries = fs
                .read_dir(Path::new(&path_str))
                .await
                .map_err(|e| map_err("fs_ls", e))?;

            let json = serde_json::Value::Array(
                entries
                    .iter()
                    .map(|e| {
                        let kind = match e.metadata.file_type {
                            bashkit::FileType::File => "file",
                            bashkit::FileType::Directory => "directory",
                            bashkit::FileType::Symlink => "symlink",
                        };
                        serde_json::json!({
                            "name": e.name,
                            "type": kind,
                            "size": e.metadata.size,
                        })
                    })
                    .collect(),
            );

            Ok(ToolResult::Output(stream! {
                yield ToolOutput::text(json.to_string());
            }))
        }
    }
}

// ── FsToolkit ─────────────────────────────────────────────────────────────────

/// A group of five filesystem tools that share a single [`bashkit::FileSystem`].
///
/// All tools handed out by `FsToolkit` operate on the **same** underlying
/// storage, so writes from `fs_write` are immediately visible to `fs_read`,
/// `fs_ls`, etc.
///
/// # Creating a toolkit
///
/// ```rust,no_run
/// use remi_tool::bkfs::FsToolkit;
/// use bashkit::InMemoryFs;
/// use std::sync::Arc;
///
/// // Pure in-memory virtual filesystem
/// let kit = FsToolkit::new(Arc::new(InMemoryFs::new()));
///
/// // When `tool-bash-virtual` is also enabled, build from FsMode:
/// // let kit = FsToolkit::new(FsMode::Local { root: "/tmp/sb".into() }.build_fs()?);
/// ```
///
/// # Registering with an agent
///
/// ```rust,no_run
/// // builder is a remi_core AgentBuilder
/// builder
///     .tool(kit.read())
///     .tool(kit.write())
///     .tool(kit.mkdir())
///     .tool(kit.remove())
///     .tool(kit.ls());
/// ```
pub struct FsToolkit {
    fs: Arc<dyn bashkit::FileSystem>,
}

impl FsToolkit {
    /// Create a toolkit backed by any [`bashkit::FileSystem`] implementation.
    pub fn new(fs: Arc<dyn bashkit::FileSystem>) -> Self {
        Self { fs }
    }

    /// Return a clone of the underlying `Arc<dyn FileSystem>`.
    ///
    /// Useful for sharing the same filesystem with a [`VirtualBashTool`].
    pub fn fs(&self) -> Arc<dyn bashkit::FileSystem> {
        self.fs.clone()
    }

    /// Returns the `fs_read` tool.
    pub fn read(&self) -> FsReadTool {
        FsReadTool::new(self.fs.clone())
    }

    /// Returns the `fs_write` tool.
    pub fn write(&self) -> FsWriteTool {
        FsWriteTool::new(self.fs.clone())
    }

    /// Returns the `fs_mkdir` tool.
    pub fn mkdir(&self) -> FsCreateTool {
        FsCreateTool::new(self.fs.clone())
    }

    /// Returns the `fs_remove` tool.
    pub fn remove(&self) -> FsRemoveTool {
        FsRemoveTool::new(self.fs.clone())
    }

    /// Returns the `fs_ls` tool.
    pub fn ls(&self) -> FsLsTool {
        FsLsTool::new(self.fs.clone())
    }
}
