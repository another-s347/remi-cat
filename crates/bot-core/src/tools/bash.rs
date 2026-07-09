use std::sync::Arc;
use std::time::Instant;

use async_stream::stream;
use bot_runtime_core::ToolContext;
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{AgentError, ResumePayload, Tool, ToolOutput, ToolResult};

use crate::sandbox::{Sandbox, SandboxBashOutput, SandboxBashStatus};

use super::{bash_task_json, format_bash_text, json_text, log_preview, SharedRedactor};

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
         and preserve state such as cd and exported variables. Long-running \
         commands are managed by the background tool task system. Prefer \
         straightforward commands that complete synchronously; if the invoked \
         program supports its own timeout flag, choose a generous timeout."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        let mut props = serde_json::json!({
            "command":    { "type": "string",  "description": "Shell command to execute" },
            "named":      { "type": "string",  "description": "Optional named shell session. Calls with the same name preserve shell state." }
        });
        if !super::async_agent_enabled() {
            props["timeout"] = serde_json::json!({ "type": "integer", "description": "Optional timeout in seconds. If the program supports its own timeout flag, prefer that and set this to a generous value." });
        }
        serde_json::json!({
            "type": "object",
            "properties": props,
            "required": ["command"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: ToolContext,
    ) -> impl std::future::Future<
        Output = Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
    > {
        let sandbox = Arc::clone(&self.sandbox);
        let redactor = Arc::clone(&self.redactor);
        async move {
            let command = arguments["command"]
                .as_str()
                .ok_or_else(|| AgentError::tool("bash", "missing 'command'"))?
                .to_string();
            let named = arguments["named"]
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
            let timeout_ms = arguments["timeout"]
                .as_u64()
                .filter(|v| *v > 0)
                .map(|s| s.saturating_mul(1000))
                .unwrap_or(u64::MAX / 2);
            let cancel = Some(ctx.runtime().cancellation());
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
                match sandbox.bash(&command, named.as_deref(), timeout_ms, cancel).await {
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
