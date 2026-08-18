use std::sync::Arc;
use std::time::Instant;

use async_stream::stream;
use bot_runtime_core::ToolContext;
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{AgentError, ResumePayload, Tool, ToolOutput, ToolResult};

use crate::sandbox::{Sandbox, SandboxBashOutput, SandboxBashStatus};

use super::{bash_task_json, format_bash_text, json_text, log_preview, SharedRedactor};

const BASH_OUTPUT_POLL_INTERVAL_MS: u64 = 100;

fn drain_complete_lines(buffer: &mut String, flush: bool) -> String {
    let end = if flush {
        buffer.len()
    } else {
        buffer.rfind('\n').map(|index| index + 1).unwrap_or(0)
    };
    if end == 0 {
        return String::new();
    }
    let remainder = buffer.split_off(end);
    std::mem::replace(buffer, remainder)
}

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

fn bash_timeout_ms(arguments: &serde_json::Value) -> u64 {
    arguments["timeout"]
        .as_u64()
        .or_else(|| arguments["timeout"].as_str()?.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(|seconds| seconds.saturating_mul(1000))
        .unwrap_or(u64::MAX / 2)
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
        #[cfg(feature = "experimental-pty")]
        {
            props["experimental_pty"] = serde_json::json!({
                "type": "boolean",
                "description": "Experimental: run in a PTY and return the rendered VT100 screen instead of raw terminal control bytes. Named sessions are not supported."
            });
            props["pty_rows"] = serde_json::json!({
                "type": "integer", "minimum": 5, "maximum": 100, "default": 24,
                "description": "PTY screen height when experimental_pty is enabled"
            });
            props["pty_cols"] = serde_json::json!({
                "type": "integer", "minimum": 20, "maximum": 300, "default": 80,
                "description": "PTY screen width when experimental_pty is enabled"
            });
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
            let timeout_ms = bash_timeout_ms(&arguments);
            let cancel = ctx.runtime().cancellation();
            #[cfg(feature = "experimental-pty")]
            let experimental_pty = arguments["experimental_pty"].as_bool().unwrap_or(false);
            #[cfg(feature = "experimental-pty")]
            let pty_rows = arguments["pty_rows"].as_u64().unwrap_or(24).clamp(5, 100) as u16;
            #[cfg(feature = "experimental-pty")]
            let pty_cols = arguments["pty_cols"].as_u64().unwrap_or(80).clamp(20, 300) as u16;
            #[cfg(feature = "experimental-pty")]
            if experimental_pty && named.is_some() {
                return Err(AgentError::tool(
                    "bash",
                    "experimental_pty cannot be combined with a named shell session",
                ));
            }
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
                #[cfg(feature = "experimental-pty")]
                if experimental_pty {
                    let pty_cancel = cancel.child_token();
                    let deadline = tokio::time::Instant::now()
                        + std::time::Duration::from_millis(timeout_ms.min(31_536_000_000));
                    match sandbox
                        .bash_pty(&command, pty_rows, pty_cols, pty_cancel.clone())
                        .await
                    {
                        Err(error) => {
                            yield ToolOutput::text(format!("error: {error:#}"));
                        }
                        Ok(mut process) => {
                            let mut parser = vt100::Parser::new(pty_rows, pty_cols, 0);
                            let mut last_screen = String::new();
                            let mut timed_out = false;
                            let mut output_open = true;
                            let exit_code = loop {
                                tokio::select! {
                                    chunk = process.output_rx.recv(), if output_open => {
                                        match chunk {
                                            Some(chunk) => {
                                                parser.process(&chunk);
                                                let screen = parser.screen().contents();
                                                let screen = redactor.read().unwrap().redact(&screen);
                                                if screen != last_screen {
                                                    last_screen = screen.clone();
                                                    yield ToolOutput::custom(
                                                        "remi.experimental_pty.screen",
                                                        serde_json::json!({ "screen": screen }),
                                                    );
                                                }
                                            }
                                            None => output_open = false,
                                        }
                                    }
                                    result = &mut process.exit_rx => {
                                        break match result {
                                            Ok(Ok(code)) => code.min(i32::MAX as u32) as i32,
                                            Ok(Err(error)) => {
                                                yield ToolOutput::text(format!("error: waiting for PTY command: {error}"));
                                                return;
                                            }
                                            Err(_) => {
                                                yield ToolOutput::text("error: PTY command exit channel closed");
                                                return;
                                            }
                                        };
                                    }
                                    _ = tokio::time::sleep_until(deadline), if !timed_out => {
                                        timed_out = true;
                                        pty_cancel.cancel();
                                    }
                                }
                            };
                            let drain_deadline = tokio::time::Instant::now()
                                + std::time::Duration::from_millis(100);
                            while output_open {
                                tokio::select! {
                                    chunk = process.output_rx.recv() => {
                                        match chunk {
                                            Some(chunk) => parser.process(&chunk),
                                            None => output_open = false,
                                        }
                                    }
                                    _ = tokio::time::sleep_until(drain_deadline) => break,
                                }
                            }
                            let screen = redactor
                                .read()
                                .unwrap()
                                .redact(&parser.screen().contents());
                            if screen != last_screen {
                                yield ToolOutput::custom(
                                    "remi.experimental_pty.screen",
                                    serde_json::json!({ "screen": screen }),
                                );
                            }
                            let status = if timed_out {
                                format!("[PTY timed out; exit code {exit_code}]")
                            } else {
                                format!("[PTY exited with code {exit_code}]")
                            };
                            let result = if screen.is_empty() {
                                status
                            } else {
                                format!("{screen}\n\n{status}")
                            };
                            yield ToolOutput::text(result);
                        }
                    }
                    return;
                }
                let first_wait_ms = timeout_ms.min(BASH_OUTPUT_POLL_INTERVAL_MS);
                match sandbox.bash(&command, named.as_deref(), first_wait_ms, Some(cancel)).await {
                    Ok(output) if output.timed_out || output.status == SandboxBashStatus::Running => {
                        let pid = output.pid.clone().unwrap_or_default();
                        let mut latest = output;
                        let mut stdout = std::mem::take(&mut latest.stdout);
                        let mut stderr = std::mem::take(&mut latest.stderr);
                        let mut stdout_pending = stdout.clone();
                        let mut stderr_pending = stderr.clone();
                        loop {
                            let stdout_delta = drain_complete_lines(&mut stdout_pending, false);
                            if !stdout_delta.is_empty() {
                                let stdout_delta = redactor.read().unwrap().redact(&stdout_delta);
                                yield ToolOutput::Delta(stdout_delta);
                            }
                            let stderr_delta = drain_complete_lines(&mut stderr_pending, false);
                            if !stderr_delta.is_empty() {
                                let stderr_delta = redactor.read().unwrap().redact(&stderr_delta);
                                yield ToolOutput::Delta(format!("[stderr] {stderr_delta}"));
                            }

                            if latest.status == SandboxBashStatus::Running
                                && started.elapsed().as_millis() as u64 >= timeout_ms
                            {
                                latest = match sandbox.bash_cancel(&pid).await {
                                    Ok(output) => output,
                                    Err(error) => {
                                        yield ToolOutput::text(format!("error: {error:#}"));
                                        return;
                                    }
                                };
                                stdout.push_str(&latest.stdout);
                                stderr.push_str(&latest.stderr);
                                stdout_pending.push_str(&latest.stdout);
                                stderr_pending.push_str(&latest.stderr);
                                continue;
                            }
                            if latest.status != SandboxBashStatus::Running {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(
                                BASH_OUTPUT_POLL_INTERVAL_MS,
                            ))
                            .await;
                            latest = match sandbox.bash_poll(&pid).await {
                                Ok(output) => output,
                                Err(error) => {
                                    yield ToolOutput::text(format!("error: {error:#}"));
                                    return;
                                }
                            };
                            stdout.push_str(&latest.stdout);
                            stderr.push_str(&latest.stderr);
                            stdout_pending.push_str(&latest.stdout);
                            stderr_pending.push_str(&latest.stderr);
                        }

                        let stdout_delta = drain_complete_lines(&mut stdout_pending, true);
                        if !stdout_delta.is_empty() {
                            let stdout_delta = redactor.read().unwrap().redact(&stdout_delta);
                            yield ToolOutput::Delta(stdout_delta);
                        }
                        let stderr_delta = drain_complete_lines(&mut stderr_pending, true);
                        if !stderr_delta.is_empty() {
                            let stderr_delta = redactor.read().unwrap().redact(&stderr_delta);
                            yield ToolOutput::Delta(format!("[stderr] {stderr_delta}"));
                        }

                        let structured = latest.status != SandboxBashStatus::Completed;
                        if !structured {
                            yield ToolOutput::text(format_bash_text(
                                stdout,
                                stderr,
                                latest.exit_code,
                                &redactor,
                            ));
                        } else {
                            latest.stdout = stdout;
                            latest.stderr = stderr;
                            let value = bash_task_json(latest, &redactor);
                            yield ToolOutput::text(json_text(value));
                        }
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

#[cfg(test)]
mod tests {
    use super::bash_timeout_ms;

    #[test]
    fn timeout_accepts_number_and_numeric_string() {
        assert_eq!(
            bash_timeout_ms(&serde_json::json!({"timeout": 300})),
            300_000
        );
        assert_eq!(
            bash_timeout_ms(&serde_json::json!({"timeout": "300"})),
            300_000
        );
    }
}
