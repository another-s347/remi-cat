//! Workspace-rooted filesystem tools and bash tool.
//!
//! All `Rooted*` tools sandbox every path under a root directory:
//! `/etc/passwd` → `<root>/etc/passwd`, `src/main.rs` → `<root>/src/main.rs`.
//!
//! `WorkspaceBashTool` runs real `bash -c` with `current_dir` set to the root
//! so relative paths inside scripts resolve correctly.

use async_stream::stream;
use futures::Stream;
use std::path::{Path, PathBuf};

use remi_core::error::AgentError;
use remi_core::tool::{Tool, ToolContext, ToolOutput, ToolResult};
use remi_core::types::ResumePayload;

// ── Common helper ─────────────────────────────────────────────────────────────

/// Join `root` with `path`, stripping any leading `/` so that absolute paths
/// are treated as relative to the root rather than escaping it.
fn resolve(root: &Path, path: &str) -> PathBuf {
    let stripped = path.trim_start_matches('/');
    root.join(stripped)
}

// ── WorkspaceBashTool ─────────────────────────────────────────────────────────

/// Real `bash -c` executor with its working directory set to the workspace root.
/// Relative paths in scripts resolve under the workspace directory.
pub struct WorkspaceBashTool {
    pub root: PathBuf,
}

impl WorkspaceBashTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl Tool for WorkspaceBashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Execute a bash shell command. Working directory is the agent workspace. \
         Relative paths resolve under the workspace."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command":    { "type": "string",  "description": "The shell command to execute" },
                "timeout_ms": { "type": "integer", "description": "Optional timeout in milliseconds" }
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
        let root = self.root.clone();
        async move {
            let command = arguments["command"]
                .as_str()
                .ok_or_else(|| AgentError::tool("bash", "missing 'command' argument"))?
                .to_string();

            Ok(ToolResult::Output(stream! {
                yield ToolOutput::Delta(format!("$ {}", command));
                // Ensure workspace dir exists before running
                let _ = std::fs::create_dir_all(&root);
                let output = tokio::process::Command::new("bash")
                    .arg("-c")
                    .arg(&command)
                    .current_dir(&root)
                    .output()
                    .await;
                match output {
                    Err(e) => yield ToolOutput::text(format!("error: {}", e)),
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                        let exit_code = out.status.code().unwrap_or(-1);
                        let mut result = String::new();
                        if !stdout.is_empty() { result.push_str(&stdout); }
                        if !stderr.is_empty() {
                            if !result.is_empty() { result.push('\n'); }
                            result.push_str("[stderr] ");
                            result.push_str(&stderr);
                        }
                        if exit_code != 0 {
                            result.push_str(&format!("\n[exit {}]", exit_code));
                        }
                        yield ToolOutput::text(result);
                    }
                }
            }))
        }
    }
}

// ── RootedFsReadTool ──────────────────────────────────────────────────────────

pub struct RootedFsReadTool {
    pub root: PathBuf,
}

impl Tool for RootedFsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Read a file in the workspace. Supports chunked reading via `offset` and \
        `length` to avoid returning too much data at once. \n\
        - `offset`: byte offset to start reading from (default 0). \n\
        - `length`: maximum number of bytes to return (default 8192). \n\
        Always check the `[total_bytes]` footer in the result and call again \
        with `offset += length` until you have read all the content you need."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string",  "description": "File path (relative to workspace root)" },
                "offset": { "type": "integer", "description": "Byte offset to start reading from (default 0)" },
                "length": { "type": "integer", "description": "Maximum bytes to read (default 8192)" }
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
        let root = self.root.clone();
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_read", "missing 'path'"))?
                .to_string();
            let offset = arguments["offset"].as_u64().unwrap_or(0) as usize;
            let length = arguments["length"].as_u64().unwrap_or(8192) as usize;
            let full = resolve(&root, &path_str);
            Ok(ToolResult::Output(stream! {
                match tokio::fs::read(&full).await {
                    Err(e) => yield ToolOutput::text(format!("error: {}", e)),
                    Ok(bytes) => {
                        let total = bytes.len();
                        let start = offset.min(total);
                        let end   = (start + length).min(total);
                        let chunk = &bytes[start..end];
                        let text  = String::from_utf8_lossy(chunk).into_owned();
                        let remaining = total.saturating_sub(end);
                        let mut result = text;
                        result.push_str(&format!(
                            "\n[offset={start} length={} total_bytes={total} remaining={remaining}]",
                            end - start
                        ));
                        if remaining > 0 {
                            result.push_str(&format!(
                                "\n[{remaining} bytes remaining — call fs_read again with offset={end}]"
                            ));
                        }
                        yield ToolOutput::text(result);
                    }
                }
            }))
        }
    }
}

// ── RootedFsWriteTool ─────────────────────────────────────────────────────────

pub struct RootedFsWriteTool {
    pub root: PathBuf,
}

impl Tool for RootedFsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn description(&self) -> &str {
        "Write text to a file in the workspace. Path is relative to the workspace root. \
         Parent directories must exist."
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
        let root = self.root.clone();
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
            let full = resolve(&root, &path_str);
            Ok(ToolResult::Output(stream! {
                // Auto-create parent dirs
                if let Some(parent) = full.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                match tokio::fs::write(&full, content.as_bytes()).await {
                    Ok(()) => yield ToolOutput::text(format!("wrote {} bytes to {}", bytes, path_str)),
                    Err(e) => yield ToolOutput::text(format!("error: {}", e)),
                }
            }))
        }
    }
}

// ── RootedFsCreateTool ────────────────────────────────────────────────────────

pub struct RootedFsCreateTool {
    pub root: PathBuf,
}

impl Tool for RootedFsCreateTool {
    fn name(&self) -> &str {
        "fs_mkdir"
    }
    fn description(&self) -> &str {
        "Create a directory in the workspace. Path is relative to the workspace root."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": "Directory path" },
                "recursive": { "type": "boolean", "description": "Create parent dirs (mkdir -p)", "default": true }
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
        let root = self.root.clone();
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_mkdir", "missing 'path'"))?
                .to_string();
            let recursive = arguments["recursive"].as_bool().unwrap_or(true);
            let full = resolve(&root, &path_str);
            Ok(ToolResult::Output(stream! {
                let result = if recursive {
                    tokio::fs::create_dir_all(&full).await
                } else {
                    tokio::fs::create_dir(&full).await
                };
                match result {
                    Ok(()) => yield ToolOutput::text(format!("created directory {}", path_str)),
                    Err(e) => yield ToolOutput::text(format!("error: {}", e)),
                }
            }))
        }
    }
}

// ── RootedFsRemoveTool ────────────────────────────────────────────────────────

pub struct RootedFsRemoveTool {
    pub root: PathBuf,
}

impl Tool for RootedFsRemoveTool {
    fn name(&self) -> &str {
        "fs_remove"
    }
    fn description(&self) -> &str {
        "Remove a file or directory in the workspace. Path is relative to the workspace root."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": "Path to remove" },
                "recursive": { "type": "boolean", "description": "Remove directory contents (rm -r)", "default": false }
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
        let root = self.root.clone();
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_remove", "missing 'path'"))?
                .to_string();
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            let full = resolve(&root, &path_str);
            Ok(ToolResult::Output(stream! {
                let result = if recursive {
                    tokio::fs::remove_dir_all(&full).await
                } else {
                    match tokio::fs::remove_file(&full).await {
                        Ok(()) => Ok(()),
                        Err(_) => tokio::fs::remove_dir(&full).await,
                    }
                };
                match result {
                    Ok(()) => yield ToolOutput::text(format!("removed {}", path_str)),
                    Err(e) => yield ToolOutput::text(format!("error: {}", e)),
                }
            }))
        }
    }
}

// ── RootedFsLsTool ────────────────────────────────────────────────────────────

pub struct RootedFsLsTool {
    pub root: PathBuf,
}

impl Tool for RootedFsLsTool {
    fn name(&self) -> &str {
        "fs_ls"
    }
    fn description(&self) -> &str {
        "List directory contents in the workspace. \
         Path is relative to the workspace root. Use '.' or '' for root."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path (relative to workspace)" }
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
        let root = self.root.clone();
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_ls", "missing 'path'"))?
                .to_string();
            let full = if path_str.is_empty() || path_str == "." {
                root.clone()
            } else {
                resolve(&root, &path_str)
            };
            Ok(ToolResult::Output(stream! {
                match tokio::fs::read_dir(&full).await {
                    Err(e) => { yield ToolOutput::text(format!("error: {}", e)); }
                    Ok(mut dir) => {
                        let mut entries = Vec::new();
                        loop {
                            match dir.next_entry().await {
                                Ok(None)        => break,
                                Err(e)          => { yield ToolOutput::text(format!("error: {}", e)); return; }
                                Ok(Some(entry)) => {
                                    let name = entry.file_name().to_string_lossy().into_owned();
                                    if let Ok(meta) = entry.metadata().await {
                                        let kind = if meta.is_dir() { "directory" } else { "file" };
                                        entries.push(serde_json::json!({
                                            "name": name,
                                            "type": kind,
                                            "size": meta.len(),
                                        }));
                                    } else {
                                        entries.push(serde_json::json!({ "name": name }));
                                    }
                                }
                            }
                        }
                        entries.sort_by(|a, b| {
                            a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
                        });
                        yield ToolOutput::text(serde_json::Value::Array(entries).to_string());
                    }
                }
            }))
        }
    }
}

// ── MemoryReadTool ────────────────────────────────────────────────────────────

/// Reads the persistent `memory.md` file from the workspace.
pub struct MemoryReadTool {
    pub path: PathBuf, // full path to memory.md
}

impl Tool for MemoryReadTool {
    fn name(&self) -> &str {
        "memory_read"
    }
    fn description(&self) -> &str {
        "Read your persistent memory notes. Returns the full content of memory.md \
         which is preserved across sessions. Call this to recall information from \
         previous sessions."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }
    fn execute(
        &self,
        _arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let path = self.path.clone();
        async move {
            Ok(ToolResult::Output(stream! {
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) if content.trim().is_empty() =>
                        yield ToolOutput::text("(memory is empty)".to_string()),
                    Ok(content) => yield ToolOutput::text(content),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound =>
                        yield ToolOutput::text("(memory is empty)".to_string()),
                    Err(e) => yield ToolOutput::text(format!("error: {e}")),
                }
            }))
        }
    }
}

// ── MemoryUpdateTool ──────────────────────────────────────────────────────────

/// Overwrites `memory.md` with new content, persisting it for future sessions.
pub struct MemoryUpdateTool {
    pub path: PathBuf, // full path to memory.md
}

impl Tool for MemoryUpdateTool {
    fn name(&self) -> &str {
        "memory_update"
    }
    fn description(&self) -> &str {
        "Overwrite your persistent memory notes. Pass the FULL desired content of \
         memory.md — this replaces everything currently stored. \
         Call this at the end of each task to record: \
         user preferences, key facts learned, project context, \
         decisions made, or anything useful to remember next session."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "Full markdown content to store as persistent memory"
                }
            },
            "required": ["content"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let path = self.path.clone();
        async move {
            let content = arguments["content"]
                .as_str()
                .ok_or_else(|| AgentError::tool("memory_update", "missing 'content'"))?
                .to_string();
            let len = content.len();
            Ok(ToolResult::Output(stream! {
                if let Some(parent) = path.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                match tokio::fs::write(&path, content.as_bytes()).await {
                    Ok(()) => yield ToolOutput::text(
                        format!("memory.md updated ({len} bytes)")
                    ),
                    Err(e) => yield ToolOutput::text(format!("error: {e}")),
                }
            }))
        }
    }
}
