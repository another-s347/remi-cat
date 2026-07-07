//! Filesystem tools that operate directly on the host filesystem via `tokio::fs`.
//!
//! Five focused tools: `fs_read`, `fs_write`, `fs_mkdir`, `fs_remove`, `fs_ls`.
//! Use [`LocalFsToolkit`] to create all five at once.

use async_stream::stream;
use futures::Stream;
use remi_core::error::AgentError;
use remi_core::tool::{Tool, ToolContext, ToolOutput, ToolResult};
use remi_core::types::ResumePayload;

// ── LocalFsReadTool ───────────────────────────────────────────────────────────

/// Reads a file from the host filesystem and returns its content as UTF-8.
pub struct LocalFsReadTool;

impl Tool for LocalFsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Read the full contents of a file on the host filesystem and return it as UTF-8."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path of the file to read" }
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
        async move {
            let path = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_read", "missing 'path'"))?
                .to_string();
            Ok(ToolResult::Output(stream! {
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => yield ToolOutput::text(content),
                    Err(e)      => yield ToolOutput::text(format!("error: {}", e)),
                }
            }))
        }
    }
}

// ── LocalFsWriteTool ──────────────────────────────────────────────────────────

/// Writes (or overwrites) a file on the host filesystem.
pub struct LocalFsWriteTool;

impl Tool for LocalFsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn description(&self) -> &str {
        "Write text content to a file on the host filesystem, creating or overwriting it. \
         Parent directory must exist."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Path of the file to write" },
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
        async move {
            let path = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_write", "missing 'path'"))?
                .to_string();
            let content = arguments["content"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_write", "missing 'content'"))?
                .to_string();
            let bytes = content.len();
            Ok(ToolResult::Output(stream! {
                match tokio::fs::write(&path, content.as_bytes()).await {
                    Ok(()) => yield ToolOutput::text(format!("wrote {} bytes to {}", bytes, path)),
                    Err(e) => yield ToolOutput::text(format!("error: {}", e)),
                }
            }))
        }
    }
}

// ── LocalFsCreateTool ─────────────────────────────────────────────────────────

/// Creates a directory on the host filesystem.
pub struct LocalFsCreateTool;

impl Tool for LocalFsCreateTool {
    fn name(&self) -> &str {
        "fs_mkdir"
    }
    fn description(&self) -> &str {
        "Create a directory on the host filesystem. \
         Set recursive=true to create all missing parent directories (mkdir -p)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": "Directory path to create" },
                "recursive": { "type": "boolean", "description": "Create parent dirs as needed", "default": false }
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
        async move {
            let path = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_mkdir", "missing 'path'"))?
                .to_string();
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            Ok(ToolResult::Output(stream! {
                let result = if recursive {
                    tokio::fs::create_dir_all(&path).await
                } else {
                    tokio::fs::create_dir(&path).await
                };
                match result {
                    Ok(()) => yield ToolOutput::text(format!("created directory {}", path)),
                    Err(e) => yield ToolOutput::text(format!("error: {}", e)),
                }
            }))
        }
    }
}

// ── LocalFsRemoveTool ─────────────────────────────────────────────────────────

/// Removes a file or directory from the host filesystem.
pub struct LocalFsRemoveTool;

impl Tool for LocalFsRemoveTool {
    fn name(&self) -> &str {
        "fs_remove"
    }
    fn description(&self) -> &str {
        "Remove a file or directory from the host filesystem. \
         Set recursive=true to remove a non-empty directory and all its contents."
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
        async move {
            let path = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_remove", "missing 'path'"))?
                .to_string();
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            Ok(ToolResult::Output(stream! {
                let result = if recursive {
                    tokio::fs::remove_dir_all(&path).await
                } else {
                    // Try file first; fall back to empty-dir removal
                    match tokio::fs::remove_file(&path).await {
                        Ok(()) => Ok(()),
                        Err(_) => tokio::fs::remove_dir(&path).await,
                    }
                };
                match result {
                    Ok(()) => yield ToolOutput::text(format!("removed {}", path)),
                    Err(e) => yield ToolOutput::text(format!("error: {}", e)),
                }
            }))
        }
    }
}

// ── LocalFsLsTool ─────────────────────────────────────────────────────────────

/// Lists directory contents on the host filesystem.
pub struct LocalFsLsTool;

impl Tool for LocalFsLsTool {
    fn name(&self) -> &str {
        "fs_ls"
    }
    fn description(&self) -> &str {
        "List the contents of a directory on the host filesystem. \
         Returns a JSON array of entries with name, type (file/directory), and size in bytes."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path to list" }
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
        async move {
            let path = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_ls", "missing 'path'"))?
                .to_string();
            Ok(ToolResult::Output(stream! {
                match tokio::fs::read_dir(&path).await {
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
                        yield ToolOutput::text(serde_json::Value::Array(entries).to_string());
                    }
                }
            }))
        }
    }
}

// ── LocalFsToolkit ────────────────────────────────────────────────────────────

/// Convenience group for all five local-filesystem tools.
///
/// ```rust,no_run
/// use remi_tool::fs::LocalFsToolkit;
///
/// let kit = LocalFsToolkit;
/// // builder.tool(kit.read()).tool(kit.write()).tool(kit.mkdir())
/// //        .tool(kit.remove()).tool(kit.ls());
/// ```
pub struct LocalFsToolkit;

impl LocalFsToolkit {
    pub fn read(&self) -> LocalFsReadTool {
        LocalFsReadTool
    }
    pub fn write(&self) -> LocalFsWriteTool {
        LocalFsWriteTool
    }
    pub fn mkdir(&self) -> LocalFsCreateTool {
        LocalFsCreateTool
    }
    pub fn remove(&self) -> LocalFsRemoveTool {
        LocalFsRemoveTool
    }
    pub fn ls(&self) -> LocalFsLsTool {
        LocalFsLsTool
    }
}
