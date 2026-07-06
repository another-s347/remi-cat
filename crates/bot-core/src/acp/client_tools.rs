use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use agent_client_protocol::schema::v1::{
    CreateTerminalRequest, ReadTextFileRequest, ReleaseTerminalRequest, SessionId,
    TerminalOutputRequest, WaitForTerminalExitRequest, WriteTextFileRequest,
};
use agent_client_protocol::{Client, ConnectionTo};
use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    AgentError, ResumePayload, Tool, ToolContext, ToolOutput, ToolResult,
};
use serde::Deserialize;

use crate::tools::DEFAULT_FS_READ_LENGTH;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AcpClientToolSupport {
    pub fs_read: bool,
    pub fs_write: bool,
    pub terminal: bool,
}

#[derive(Clone, Default)]
pub struct AcpClientToolProvider {
    state: Arc<RwLock<Option<AcpClientToolState>>>,
}

#[derive(Clone)]
struct AcpClientToolState {
    connection: ConnectionTo<Client>,
    session_id: SessionId,
    cwd: PathBuf,
}

impl AcpClientToolProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_turn(&self, connection: ConnectionTo<Client>, session_id: SessionId, cwd: PathBuf) {
        *self.state.write().unwrap() = Some(AcpClientToolState {
            connection,
            session_id,
            cwd,
        });
    }

    pub fn clear_turn(&self) {
        *self.state.write().unwrap() = None;
    }

    pub fn register_tools(
        &self,
        registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry,
        support: AcpClientToolSupport,
    ) {
        if support.fs_read {
            registry.register(AcpFsReadTool {
                provider: self.clone(),
            });
        }
        if support.fs_write {
            registry.register(AcpFsWriteTool {
                provider: self.clone(),
            });
        }
        if support.terminal {
            registry.register(AcpBashTool {
                provider: self.clone(),
            });
        }
    }

    fn current(&self, tool_name: &str) -> Result<AcpClientToolState, AgentError> {
        self.state.read().unwrap().clone().ok_or_else(|| {
            AgentError::tool(tool_name, "ACP client tool is not active for this turn")
        })
    }
}

pub struct AcpFsReadTool {
    provider: AcpClientToolProvider,
}

impl Tool for AcpFsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }

    fn description(&self) -> &str {
        "Read a text file through the ACP client. Supports `path`, 1-based inclusive `start_line` + `end_line`, and byte-style `offset`/`length` only as approximate line requests. Directories and inline images are not supported by ACP fs_read."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path. Relative paths resolve against the ACP session cwd." },
                "offset": { "type": "integer", "description": "Approximate byte offset; ACP text reads are line-based, so this is converted to line 1 with a line limit." },
                "length": { "type": "integer", "description": format!("Approximate max bytes; ACP text reads are line-based (default {DEFAULT_FS_READ_LENGTH}).") },
                "start_line": { "type": "integer", "description": "1-based inclusive line number to start reading." },
                "end_line": { "type": "integer", "description": "1-based inclusive line number to stop reading." }
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
        let provider = self.provider.clone();
        async move {
            let args: FsReadArgs = serde_json::from_value(arguments).map_err(|_| {
                AgentError::tool(
                    "fs_read",
                    "expected {path, start_line?, end_line?, offset?, length?}",
                )
            })?;
            let state = provider.current("fs_read")?;
            let path = absolute_path(&state.cwd, &args.path);
            let (line, limit) = read_line_window(&args)?;
            Ok(ToolResult::Output(stream! {
                let mut request = ReadTextFileRequest::new(state.session_id.clone(), path);
                if let Some(line) = line {
                    request = request.line(Some(line));
                }
                if let Some(limit) = limit {
                    request = request.limit(Some(limit));
                }
                match state.connection.send_request(request).block_task().await {
                    Ok(response) => yield ToolOutput::text(response.content),
                    Err(error) => yield ToolOutput::text(format!("error: ACP fs/read_text_file failed: {error}")),
                }
            }.boxed()))
        }
    }
}

#[derive(Debug, Deserialize)]
struct FsReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    length: Option<u64>,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
}

fn read_line_window(args: &FsReadArgs) -> Result<(Option<u32>, Option<u32>), AgentError> {
    if args.path.trim().is_empty() {
        return Err(AgentError::tool("fs_read", "missing 'path'"));
    }
    if let Some(start) = args.start_line {
        if start == 0 {
            return Err(AgentError::tool("fs_read", "start_line must be >= 1"));
        }
        if let Some(end) = args.end_line {
            if end < start {
                return Err(AgentError::tool(
                    "fs_read",
                    "end_line must be greater than or equal to start_line",
                ));
            }
            return Ok((Some(start), Some(end - start + 1)));
        }
        return Ok((Some(start), None));
    }
    if let Some(end) = args.end_line {
        if end == 0 {
            return Err(AgentError::tool("fs_read", "end_line must be >= 1"));
        }
        return Ok((Some(1), Some(end)));
    }
    if args.offset.unwrap_or(0) > 0 {
        return Err(AgentError::tool(
            "fs_read",
            "ACP fs_read is line-based and does not support byte offset; use start_line/end_line",
        ));
    }
    let limit = args.length.map(|length| {
        let approximate_lines = (length.max(1) + 119) / 120;
        approximate_lines.clamp(1, u64::from(u32::MAX)) as u32
    });
    Ok((None, limit))
}

pub struct AcpFsWriteTool {
    provider: AcpClientToolProvider,
}

impl Tool for AcpFsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }

    fn description(&self) -> &str {
        "Write text to a file through the ACP client. Path may be relative to the ACP session cwd or absolute."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path. Relative paths resolve against the ACP session cwd." },
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
        let provider = self.provider.clone();
        async move {
            let args: FsWriteArgs = serde_json::from_value(arguments)
                .map_err(|_| AgentError::tool("fs_write", "expected {path, content}"))?;
            if args.path.trim().is_empty() {
                return Err(AgentError::tool("fs_write", "missing 'path'"));
            }
            let bytes = args.content.len();
            let state = provider.current("fs_write")?;
            let path = absolute_path(&state.cwd, &args.path);
            Ok(ToolResult::Output(stream! {
                match state.connection
                    .send_request(WriteTextFileRequest::new(state.session_id.clone(), path.clone(), args.content))
                    .block_task()
                    .await
                {
                    Ok(_) => yield ToolOutput::text(format!("wrote {bytes} bytes to {}", path.display())),
                    Err(error) => yield ToolOutput::text(format!("error: ACP fs/write_text_file failed: {error}")),
                }
            }.boxed()))
        }
    }
}

#[derive(Debug, Deserialize)]
struct FsWriteArgs {
    path: String,
    content: String,
}

pub struct AcpBashTool {
    provider: AcpClientToolProvider,
}

impl Tool for AcpBashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a one-shot command through the ACP client terminal. Named sessions, pid polling, and cancel actions are not supported by ACP bash."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "named": { "type": "string", "description": "Unsupported for ACP bash" },
                "timeout_ms": { "type": "integer", "description": "Accepted for compatibility; ACP terminal wait has no per-request timeout field" },
                "pid": { "type": "string", "description": "Unsupported for ACP bash" },
                "action": { "type": "string", "enum": ["poll", "cancel"], "description": "Unsupported for ACP bash" }
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
        let provider = self.provider.clone();
        async move {
            let args: BashArgs = serde_json::from_value(arguments)
                .map_err(|_| AgentError::tool("bash", "expected {command, timeout_ms?}"))?;
            if args
                .pid
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                return Err(AgentError::tool(
                    "bash",
                    "ACP bash does not support pid polling or cancellation",
                ));
            }
            if args
                .action
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                return Err(AgentError::tool(
                    "bash",
                    "ACP bash does not support action=poll or action=cancel",
                ));
            }
            if args
                .named
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            {
                return Err(AgentError::tool(
                    "bash",
                    "ACP bash does not support named shell sessions",
                ));
            }
            let command = args
                .command
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| AgentError::tool("bash", "missing 'command'"))?;
            let _timeout_ms = args.timeout_ms.unwrap_or(30_000);
            let state = provider.current("bash")?;
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::Delta(format!("$ {command}"));
                let created = match state.connection
                    .send_request(
                        CreateTerminalRequest::new(state.session_id.clone(), "/bin/sh")
                            .args(vec!["-lc".to_string(), command])
                            .cwd(Some(state.cwd.clone()))
                            .output_byte_limit(Some(256 * 1024)),
                    )
                    .block_task()
                    .await
                {
                    Ok(created) => created,
                    Err(error) => {
                        yield ToolOutput::text(format!("error: ACP terminal/create failed: {error}"));
                        return;
                    }
                };
                let terminal_id = created.terminal_id;
                let wait_result = state.connection
                    .send_request(WaitForTerminalExitRequest::new(state.session_id.clone(), terminal_id.clone()))
                    .block_task()
                    .await;
                if let Err(error) = wait_result {
                    let _ = state.connection
                        .send_request(ReleaseTerminalRequest::new(state.session_id.clone(), terminal_id.clone()))
                        .block_task()
                        .await;
                    yield ToolOutput::text(format!("error: ACP terminal/wait_for_exit failed: {error}"));
                    return;
                }
                let output = state.connection
                    .send_request(TerminalOutputRequest::new(state.session_id.clone(), terminal_id.clone()))
                    .block_task()
                    .await;
                let _ = state.connection
                    .send_request(ReleaseTerminalRequest::new(state.session_id.clone(), terminal_id.clone()))
                    .block_task()
                    .await;
                match output {
                    Ok(output) => {
                        let mut text = output.output;
                        if output.truncated {
                            text.push_str("\n[output truncated by ACP client]");
                        }
                        if let Some(status) = output.exit_status {
                            if let Some(code) = status.exit_code {
                                if code != 0 {
                                    text.push_str(&format!("\n[exit {code}]"));
                                }
                            } else if let Some(signal) = status.signal {
                                text.push_str(&format!("\n[signal {signal}]"));
                            }
                        }
                        yield ToolOutput::text(text);
                    }
                    Err(error) => yield ToolOutput::text(format!("error: ACP terminal/output failed: {error}")),
                }
            }.boxed()))
        }
    }
}

#[derive(Debug, Deserialize)]
struct BashArgs {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    named: Option<String>,
    #[serde(default)]
    pid: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn absolute_path(cwd: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_read_line_window_uses_inclusive_end_line() {
        let args = FsReadArgs {
            path: "src/lib.rs".to_string(),
            offset: None,
            length: None,
            start_line: Some(10),
            end_line: Some(12),
        };

        assert_eq!(read_line_window(&args).unwrap(), (Some(10), Some(3)));
    }

    #[test]
    fn fs_read_rejects_byte_offset_for_acp_client_reads() {
        let args = FsReadArgs {
            path: "src/lib.rs".to_string(),
            offset: Some(20),
            length: Some(100),
            start_line: None,
            end_line: None,
        };

        let err = read_line_window(&args).unwrap_err();
        assert!(err.to_string().contains("does not support byte offset"));
    }

    #[test]
    fn relative_paths_resolve_against_acp_cwd() {
        assert_eq!(
            absolute_path(Path::new("/work/project"), "src/main.rs"),
            PathBuf::from("/work/project/src/main.rs")
        );
        assert_eq!(
            absolute_path(Path::new("/work/project"), "/tmp/file.txt"),
            PathBuf::from("/tmp/file.txt")
        );
    }
}
