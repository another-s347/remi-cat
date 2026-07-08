//! Workspace-rooted filesystem tools, bash executor, and web search.
//!
//! All `Rooted*` tools sandbox every path under a root directory so absolute
//! paths like `/etc/passwd` map to `<root>/etc/passwd`.
//!
//! `WorkspaceBashTool` runs real `bash -c` with `current_dir` set to the root.
//!
//! `ExaSearchTool` calls the Exa Search API when `EXA_API_KEY` is set.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_stream::stream;
use base64::Engine as _;
use bot_runtime_core::ToolContext;
use futures::Stream;
use remi_agentloop::prelude::{
    AgentError, Content, ContentPart, ResumePayload, Tool, ToolOutput, ToolResult,
};
use serde::{Deserialize, Serialize};

use crate::sandbox::{Sandbox, SandboxBashOutput, SandboxBashStatus};

mod bash;
pub use bash::{BashMode, WorkspaceBashTool};

mod redactor;
pub use redactor::{SecretRedactor, SharedRedactor};

mod ssh;
pub use ssh::WorkspaceSshTool;
#[cfg(test)]
use ssh::{parse_ssh_target, ssh_command_args, validate_ssh_named, SshTarget};

mod utility;
#[cfg(test)]
use utility::{
    format_command_output, format_utc_offset, parse_manage_yourself_command, parse_timezone_spec,
};
pub use utility::{ManageYourselfTool, NowTool, SleepTool};

pub const DEFAULT_FS_READ_LENGTH: usize = 32 * 1024;

const FS_READ_LAST_STATE_KEY: &str = "__fs_read_last";

// ── helper ────────────────────────────────────────────────────────────────────

fn workspace_path_description(sandbox: &dyn Sandbox, subject: &str) -> String {
    match sandbox.kind() {
        "docker" => format!(
            "{subject}. Use workspace-relative paths, or paths under the container workspace root such as /workspace/file."
        ),
        "no_sandbox" => format!(
            "{subject}. Use workspace-relative paths. Host absolute paths are also accepted in no_sandbox mode."
        ),
        _ => format!("{subject}. Use workspace-relative paths."),
    }
}

fn log_preview(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in value.chars().take(max_chars) {
        out.push(ch);
    }
    if value.chars().count() > max_chars {
        out.push_str("...");
    }
    out.replace('\n', "\\n")
}

fn detect_inline_image_media_type(path: &str, bytes: &[u8]) -> Option<&'static str> {
    detect_inline_image_media_type_from_magic(bytes)
        .or_else(|| detect_inline_image_media_type_from_extension(path))
}

fn detect_inline_image_media_type_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn detect_inline_image_media_type_from_extension(path: &str) -> Option<&'static str> {
    let ext = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn inline_image_summary(path: &str, mime_type: &str, total_bytes: usize) -> String {
    format!(
        "Read image file {path} inline.\n[mime_type={mime_type} total_bytes={total_bytes}]\n[offset/length are ignored for auto-detected images]"
    )
}

fn format_bash_text(
    stdout: String,
    stderr: String,
    code: i32,
    redactor: &SharedRedactor,
) -> String {
    let mut r = stdout;
    if !stderr.is_empty() {
        if !r.is_empty() {
            r.push('\n');
        }
        r.push_str("[stderr] ");
        r.push_str(&stderr);
    }
    if code != 0 {
        r.push_str(&format!("\n[exit {code}]"));
    }
    redactor.read().unwrap().redact(&r)
}

fn bash_task_json(output: SandboxBashOutput, redactor: &SharedRedactor) -> serde_json::Value {
    command_task_json(output, redactor, "bash")
}

fn command_task_json(
    output: SandboxBashOutput,
    redactor: &SharedRedactor,
    tool_name: &str,
) -> serde_json::Value {
    let status = match output.status {
        SandboxBashStatus::Completed => "completed",
        SandboxBashStatus::Running => "running",
        SandboxBashStatus::Cancelled => "cancelled",
        SandboxBashStatus::NotFound => "not_found",
    };
    let stdout = redactor.read().unwrap().redact(&output.stdout);
    let stderr = redactor.read().unwrap().redact(&output.stderr);
    let mut value = serde_json::json!({
        "status": status,
        "pid": output.pid,
        "os_pid": output.os_pid,
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": if matches!(output.status, SandboxBashStatus::Completed) { Some(output.exit_code) } else { None },
        "timed_out": output.timed_out,
        "message": output.message,
    });
    if matches!(output.status, SandboxBashStatus::Running) {
        let pid = value["pid"].as_str().unwrap_or_default();
        value["poll_hint"] = serde_json::Value::String(format!(
            "Call {tool_name} again with pid={pid} and action=poll. Use action=cancel to terminate it."
        ));
    }
    value
}

fn json_text(value: serde_json::Value) -> String {
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

// ── RootedFsReadTool ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FsReadLastRecord {
    path: String,
    start: usize,
    end: usize,
    requested_length: usize,
    total_bytes: usize,
    metadata_len: u64,
    modified_ms: Option<u64>,
}

fn fs_read_duplicate_warning(ctx: &ToolContext, record: FsReadLastRecord) -> Option<String> {
    ctx.update_user_state(|state| {
        if !state.is_object() {
            *state = serde_json::json!({});
        }

        let previous = state
            .get(FS_READ_LAST_STATE_KEY)
            .and_then(|value| serde_json::from_value::<FsReadLastRecord>(value.clone()).ok());

        let warning = previous.and_then(|previous| {
            let same_file_version = previous.path == record.path
                && previous.total_bytes == record.total_bytes
                && previous.metadata_len == record.metadata_len
                && previous.modified_ms == record.modified_ms;
            let same_range = previous.start == record.start && previous.end == record.end;
            if !same_file_version || !same_range {
                return None;
            }

            let mut warning = format!(
                "warning: this fs_read request repeats a range already read in this session, and the file appears unchanged (path={}, offset={}, length={}).",
                record.path,
                record.start,
                record.end.saturating_sub(record.start)
            );
            if record.end < record.total_bytes {
                warning.push_str(&format!(" Continue with offset={} to read the next unread chunk.", record.end));
            }
            Some(warning)
        });

        if let Some(object) = state.as_object_mut() {
            if let Ok(value) = serde_json::to_value(record) {
                object.insert(FS_READ_LAST_STATE_KEY.to_string(), value);
            }
        }

        warning
    })
}

fn fs_read_line_range(
    arguments: &serde_json::Value,
) -> Result<Option<(usize, Option<usize>)>, AgentError> {
    let start_line = fs_read_line_arg(arguments, "start_line")?;
    let end_line = fs_read_line_arg(arguments, "end_line")?;
    if start_line.is_none() && end_line.is_none() {
        return Ok(None);
    }

    let start_line = start_line.unwrap_or(1);
    if let Some(end_line) = end_line {
        if end_line < start_line {
            return Err(AgentError::tool(
                "fs_read",
                "end_line must be greater than or equal to start_line",
            ));
        }
    }

    Ok(Some((start_line, end_line)))
}

fn fs_read_line_arg(
    arguments: &serde_json::Value,
    key: &'static str,
) -> Result<Option<usize>, AgentError> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(number) = value.as_i64() else {
        return Err(AgentError::tool(
            "fs_read",
            format!("{key} must be a positive integer"),
        ));
    };
    if number < 1 {
        return Err(AgentError::tool("fs_read", format!("{key} must be >= 1")));
    }
    Ok(Some(number as usize))
}

fn fs_read_lines_text(
    bytes: &[u8],
    start_line: usize,
    end_line: Option<usize>,
) -> (String, usize, usize) {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();
    let requested_end = end_line.unwrap_or(total_lines);
    let start_index = start_line.saturating_sub(1).min(total_lines);
    let end_index = requested_end.min(total_lines);
    let selected = if start_index >= end_index {
        String::new()
    } else {
        lines[start_index..end_index].join("\n")
    };
    (selected, end_index, total_lines)
}

pub struct RootedFsReadTool {
    pub sandbox: Arc<dyn Sandbox>,
    pub redactor: SharedRedactor,
}

impl Tool for RootedFsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Read a file or directory in the workspace. Text files support `offset` + `length` for byte chunks, \
         or 1-based inclusive `start_line` + `end_line` line ranges. \
         Common images (JPEG, PNG, GIF, WebP) are auto-detected and returned inline as images; \
         for those files range arguments are ignored. Always check `[total_bytes]` in byte text results \
         and call again with offset += length if needed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        let path_description =
            workspace_path_description(self.sandbox.as_ref(), "File or directory path");
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string",  "description": path_description },
                "offset": { "type": "integer", "description": "Byte offset to start for text files (default 0)" },
                "length": {
                    "type": "integer",
                    "description": format!("Max bytes to return for text files (default {DEFAULT_FS_READ_LENGTH})")
                },
                "start_line": { "type": "integer", "description": "1-based inclusive line number to start reading text files. Takes precedence over offset/length." },
                "end_line": { "type": "integer", "description": "1-based inclusive line number to stop reading text files. If omitted with start_line, reads to EOF. If provided without start_line, starts at line 1." }
            },
            "required": ["path"]
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
        let tool_ctx = ctx.clone();
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_read", "missing 'path'"))?
                .to_string();
            let offset = arguments["offset"].as_u64().unwrap_or(0) as usize;
            let length = arguments["length"]
                .as_u64()
                .unwrap_or(DEFAULT_FS_READ_LENGTH as u64) as usize;
            let line_range = fs_read_line_range(&arguments)?;
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    offset,
                    length,
                    start_line = line_range.map(|(start, _)| start).unwrap_or_default(),
                    end_line = line_range.and_then(|(_, end)| end).unwrap_or_default(),
                    sandbox_kind = %sandbox.kind(),
                    "fs_read.start"
                );
                let metadata = match sandbox.metadata(&path_str).await {
                    Ok(metadata) => Some(metadata),
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            sandbox_kind = %sandbox.kind(),
                            error = %e,
                            "fs_read.metadata.failed"
                        );
                        None
                    }
                };
                if metadata.as_ref().is_some_and(|metadata| metadata.is_dir) {
                    match sandbox.list(&path_str).await {
                        Err(e) => {
                            tracing::warn!(
                                path = %path_str,
                                sandbox_kind = %sandbox.kind(),
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                error = %e,
                                "fs_read.failed"
                            );
                            yield ToolOutput::text(format!("error: {e:#}"));
                        }
                        Ok(entries) => {
                            tracing::info!(
                                path = %path_str,
                                entries = entries.len(),
                                directory = true,
                                sandbox_kind = %sandbox.kind(),
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "fs_read.completed"
                            );
                            let listing = redactor.read().unwrap().redact(&entries.join("\n"));
                            let mut output = format!("{path_str} is a directory; use fs_ls for directory-focused listing.");
                            if !listing.is_empty() {
                                output.push('\n');
                                output.push_str(&listing);
                            }
                            yield ToolOutput::text(output);
                        }
                    }
                    return;
                }
                match sandbox.read(&path_str).await {
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            offset,
                            length,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_read.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                    Ok(bytes) => {
                        let total = bytes.len();
                        if let Some(mime_type) = detect_inline_image_media_type(&path_str, &bytes) {
                            tracing::info!(
                                path = %path_str,
                                total_bytes = total,
                                mime_type,
                                inline_image = true,
                                sandbox_kind = %sandbox.kind(),
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "fs_read.completed"
                            );
                            let summary = redactor
                                .read()
                                .unwrap()
                                .redact(&inline_image_summary(&path_str, mime_type, total));
                            let data_url = format!(
                                "data:{mime_type};base64,{}",
                                base64::engine::general_purpose::STANDARD.encode(&bytes)
                            );
                            yield ToolOutput::Result(Content::parts(vec![
                                ContentPart::text(summary),
                                ContentPart::image_url(data_url),
                            ]));
                            return;
                        }

                        if let Some((start_line, end_line)) = line_range {
                            let (text, returned_end_line, total_lines) =
                                fs_read_lines_text(&bytes, start_line, end_line);
                            tracing::info!(
                                path = %path_str,
                                start_line,
                                end_line = returned_end_line,
                                total_lines,
                                inline_image = false,
                                sandbox_kind = %sandbox.kind(),
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                "fs_read.completed"
                            );
                            let text = redactor.read().unwrap().redact(&text);
                            let mut r = text;
                            r.push_str(&format!(
                                "\n[start_line={start_line} end_line={returned_end_line} total_lines={total_lines}]"
                            ));
                            yield ToolOutput::text(r);
                            return;
                        }

                        let start = offset.min(total);
                        let end   = (start + length).min(total);
                        let duplicate_warning = metadata.as_ref().and_then(|metadata| {
                            fs_read_duplicate_warning(
                                &tool_ctx,
                                FsReadLastRecord {
                                    path: path_str.clone(),
                                    start,
                                    end,
                                    requested_length: length,
                                    total_bytes: total,
                                    metadata_len: metadata.len,
                                    modified_ms: metadata.modified_ms,
                                },
                            )
                        });
                        let text  = String::from_utf8_lossy(&bytes[start..end]).into_owned();
                        let remaining = total.saturating_sub(end);
                        tracing::info!(
                            path = %path_str,
                            start,
                            returned_bytes = end.saturating_sub(start),
                            total_bytes = total,
                            remaining,
                            inline_image = false,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_read.completed"
                        );
                        let text = redactor.read().unwrap().redact(&text);
                        let mut r = text;
                        if let Some(warning) = duplicate_warning {
                            r = format!("{warning}\n\n{r}");
                        }
                        r.push_str(&format!("\n[offset={start} length={} total_bytes={total} remaining={remaining}]", end - start));
                        if remaining > 0 {
                            r.push_str(&format!("\n[{remaining} bytes remaining — call fs_read again with offset={end}]"));
                        }
                        yield ToolOutput::text(r);
                    }
                }
            }))
        }
    }
}

// ── RootedFsWriteTool ─────────────────────────────────────────────────────────

pub struct RootedFsWriteTool {
    pub sandbox: Arc<dyn Sandbox>,
}

impl Tool for RootedFsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn description(&self) -> &str {
        "Write text to a file in the workspace. Path is relative to workspace root. \
         Parent directories must already exist. Prefer apply_patch for editing existing \
         files; use fs_write only when creating a new file or intentionally replacing an \
         entire file after reading it."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        let path_description = workspace_path_description(self.sandbox.as_ref(), "File path");
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": path_description },
                "content": { "type": "string", "description": "Text content to write" }
            },
            "required": ["path", "content"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: ToolContext,
    ) -> impl std::future::Future<
        Output = Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
    > {
        let sandbox = Arc::clone(&self.sandbox);
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
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    bytes,
                    sandbox_kind = %sandbox.kind(),
                    "fs_write.start"
                );
                match sandbox.write(&path_str, content.as_bytes()).await {
                    Ok(()) => {
                        tracing::info!(
                            path = %path_str,
                            bytes,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_write.completed"
                        );
                        yield ToolOutput::text(format!("wrote {bytes} bytes to {path_str}"));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            bytes,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_write.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                }
            }))
        }
    }
}

// ── RootedFsApplyPatchTool ────────────────────────────────────────────────────

pub struct RootedFsApplyPatchTool {
    pub sandbox: Arc<dyn Sandbox>,
}

impl Tool for RootedFsApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> &str {
        match self.sandbox.kind() {
            "docker" => {
                "Apply a focused multi-file patch in the workspace. Prefer this for edits to \
                 existing files. The patch must be a standard unified diff, such as output \
                 from `git diff` or `diff -u`, with `---` / `+++` file headers and `@@` \
                 hunks. Every context line, including a blank one, starts with a space. \
                 `diff --git` headers are accepted. Patch file paths may be workspace-relative \
                 or under the container workspace root such as /workspace/file. Each old hunk must match exactly once."
            }
            "no_sandbox" => {
                "Apply a focused multi-file patch in the workspace. Prefer this for edits to \
                 existing files. The patch must be a standard unified diff, such as output \
                 from `git diff` or `diff -u`, with `---` / `+++` file headers and `@@` \
                 hunks. Every context line, including a blank one, starts with a space. \
                 `diff --git` headers are accepted. Patch file paths may be workspace-relative \
                 or host absolute paths. Each old hunk must match exactly once."
            }
            _ => {
                "Apply a focused multi-file patch in the workspace. Prefer this for edits to \
                 existing files. The patch must be a standard unified diff, such as output \
                 from `git diff` or `diff -u`, with `---` / `+++` file headers and `@@` \
                 hunks. Every context line, including a blank one, starts with a space. \
                 `diff --git` headers are accepted. Patch file paths should be workspace-relative. \
                 Each old hunk must match exactly once."
            }
        }
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Standard unified diff text, such as output from git diff or diff -u. Prefix blank context lines with a space."
                },
                "workdir": {
                    "type": "string",
                    "description": "Optional workspace-relative directory that patch paths are relative to, e.g. `repo` for a diff generated inside repo/"
                }
            },
            "required": ["patch"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: ToolContext,
    ) -> impl std::future::Future<
        Output = Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
    > {
        let sandbox = Arc::clone(&self.sandbox);
        async move {
            let patch = arguments["patch"]
                .as_str()
                .ok_or_else(|| AgentError::tool("apply_patch", "missing 'patch'"))?
                .to_string();
            let workdir = arguments["workdir"].as_str().map(str::to_string);
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    patch_bytes = patch.len(),
                    workdir = workdir.as_deref().unwrap_or(""),
                    sandbox_kind = %sandbox.kind(),
                    "apply_patch.start"
                );
                match apply_patch_to_sandbox(sandbox.as_ref(), &patch, workdir.as_deref()).await {
                    Ok(summary) => {
                        tracing::info!(
                            patch_bytes = patch.len(),
                            workdir = workdir.as_deref().unwrap_or(""),
                            added = summary.added.len(),
                            updated = summary.updated.len(),
                            deleted = summary.deleted.len(),
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "apply_patch.completed"
                        );
                        yield ToolOutput::text(summary.to_string());
                    }
                    Err(e) => {
                        tracing::warn!(
                            patch_bytes = patch.len(),
                            workdir = workdir.as_deref().unwrap_or(""),
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "apply_patch.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                }
            }))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedPatchOp {
    Add { path: String, content: String },
    Delete { path: String },
    Update { path: String, hunks: Vec<PatchHunk> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PatchHunk {
    old: String,
    new: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ApplyPatchSummary {
    added: Vec<String>,
    updated: Vec<String>,
    deleted: Vec<String>,
}

impl std::fmt::Display for ApplyPatchSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if !self.added.is_empty() {
            parts.push(format!(
                "added {} file(s): {}",
                self.added.len(),
                self.added.join(", ")
            ));
        }
        if !self.updated.is_empty() {
            parts.push(format!(
                "updated {} file(s): {}",
                self.updated.len(),
                self.updated.join(", ")
            ));
        }
        if !self.deleted.is_empty() {
            parts.push(format!(
                "deleted {} file(s): {}",
                self.deleted.len(),
                self.deleted.join(", ")
            ));
        }
        if parts.is_empty() {
            write!(f, "patch applied with no file changes")
        } else {
            write!(f, "patch applied: {}", parts.join("; "))
        }
    }
}

async fn apply_patch_to_sandbox(
    sandbox: &dyn Sandbox,
    patch: &str,
    workdir: Option<&str>,
) -> anyhow::Result<ApplyPatchSummary> {
    let mut ops = parse_apply_patch(patch).map_err(|err| anyhow::anyhow!(err))?;
    if let Some(raw_workdir) = workdir {
        if let Some(workdir) = normalize_patch_workdir(raw_workdir)? {
            for op in &mut ops {
                prefix_patch_op_path(op, &workdir);
            }
        }
    }
    let mut seen_paths = HashSet::new();
    let mut staged: HashMap<String, Option<String>> = HashMap::new();

    for op in &ops {
        let path = patch_op_path(op);
        if !seen_paths.insert(path.to_string()) {
            anyhow::bail!("patch contains multiple operations for {path}");
        }
        match op {
            ParsedPatchOp::Add { path, content } => {
                if sandbox.read(path).await.is_ok() {
                    anyhow::bail!("cannot add {path}: file already exists");
                }
                staged.insert(path.clone(), Some(content.clone()));
            }
            ParsedPatchOp::Delete { path } => {
                sandbox
                    .read(path)
                    .await
                    .with_context(|| format!("cannot delete {path}"))?;
                staged.insert(path.clone(), None);
            }
            ParsedPatchOp::Update { path, hunks } => {
                let bytes = sandbox
                    .read(path)
                    .await
                    .with_context(|| format!("cannot update {path}"))?;
                let mut content = String::from_utf8(bytes)
                    .with_context(|| format!("cannot update {path}: file is not UTF-8 text"))?;
                for hunk in hunks {
                    if hunk.old.is_empty() {
                        anyhow::bail!("update hunk for {path} has no old content");
                    }
                    let count = content.matches(&hunk.old).count();
                    if count == 0 {
                        anyhow::bail!("update hunk did not match {path}");
                    }
                    if count > 1 {
                        anyhow::bail!(
                            "update hunk matched {count} times in {path}; add more context"
                        );
                    }
                    content = content.replacen(&hunk.old, &hunk.new, 1);
                }
                staged.insert(path.clone(), Some(content));
            }
        }
    }

    let mut summary = ApplyPatchSummary::default();
    for op in ops {
        match op {
            ParsedPatchOp::Add { path, .. } => {
                let content = staged
                    .remove(&path)
                    .and_then(|value| value)
                    .unwrap_or_default();
                sandbox
                    .write(&path, content.as_bytes())
                    .await
                    .with_context(|| format!("adding {path}"))?;
                summary.added.push(path);
            }
            ParsedPatchOp::Delete { path } => {
                sandbox
                    .remove(&path, false)
                    .await
                    .with_context(|| format!("deleting {path}"))?;
                let _ = staged.remove(&path);
                summary.deleted.push(path);
            }
            ParsedPatchOp::Update { path, .. } => {
                let content = staged
                    .remove(&path)
                    .and_then(|value| value)
                    .unwrap_or_default();
                sandbox
                    .write(&path, content.as_bytes())
                    .await
                    .with_context(|| format!("updating {path}"))?;
                summary.updated.push(path);
            }
        }
    }

    Ok(summary)
}

fn patch_op_path(op: &ParsedPatchOp) -> &str {
    match op {
        ParsedPatchOp::Add { path, .. }
        | ParsedPatchOp::Delete { path }
        | ParsedPatchOp::Update { path, .. } => path,
    }
}

fn prefix_patch_op_path(op: &mut ParsedPatchOp, workdir: &str) {
    let path = match op {
        ParsedPatchOp::Add { path, .. }
        | ParsedPatchOp::Delete { path }
        | ParsedPatchOp::Update { path, .. } => path,
    };
    if !path.starts_with(&format!("{workdir}/")) {
        *path = format!("{workdir}/{path}");
    }
}

fn normalize_patch_workdir(workdir: &str) -> anyhow::Result<Option<String>> {
    let workdir = workdir.trim().trim_matches('/');
    if workdir.is_empty() || workdir == "." {
        return Ok(None);
    }
    if workdir
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        anyhow::bail!("invalid apply_patch workdir: {workdir}");
    }
    Ok(Some(workdir.to_string()))
}

fn parse_apply_patch(patch: &str) -> Result<Vec<ParsedPatchOp>, String> {
    parse_unified_patch(patch)
}

fn parse_unified_patch(patch: &str) -> Result<Vec<ParsedPatchOp>, String> {
    let lines: Vec<&str> = patch.split_inclusive('\n').collect();
    if lines.is_empty() {
        return Err("patch must not be empty".to_string());
    }
    let mut index = 0;
    let mut ops = Vec::new();
    while index < lines.len() {
        let line = trim_line(lines[index]);
        if line.is_empty()
            || line.starts_with("diff --git ")
            || is_unified_patch_metadata_line(line)
        {
            index += 1;
            continue;
        }

        let Some(old_path_raw) = line.strip_prefix("--- ") else {
            return Err(format!("expected unified diff file header, got: {line}"));
        };
        index += 1;
        if index >= lines.len() {
            return Err("missing +++ file header".to_string());
        }
        let new_header = trim_line(lines[index]);
        let Some(new_path_raw) = new_header.strip_prefix("+++ ") else {
            return Err(format!("expected +++ file header, got: {new_header}"));
        };
        index += 1;

        let old_path = parse_unified_path(old_path_raw)?;
        let new_path = parse_unified_path(new_path_raw)?;
        let path = match (old_path.as_deref(), new_path.as_deref()) {
            (None, None) => return Err("patch cannot use /dev/null for both paths".to_string()),
            (None, Some(path)) | (Some(path), None) | (Some(_), Some(path)) => path.to_string(),
        };

        let mut hunks = Vec::new();
        while index < lines.len() {
            let marker = trim_line(lines[index]);
            if marker.starts_with("diff --git ") || marker.starts_with("--- ") {
                break;
            }
            if is_unified_patch_metadata_line(marker) {
                index += 1;
                continue;
            }
            if !marker.starts_with("@@") {
                return Err(format!(
                    "file {path} expected @@ hunk marker, got: {marker}"
                ));
            }
            index += 1;
            let mut old = String::new();
            let mut new = String::new();
            let mut saw_line = false;
            while index < lines.len() {
                let next = trim_line(lines[index]);
                if next.starts_with("@@")
                    || next.starts_with("diff --git ")
                    || next.starts_with("--- ")
                {
                    break;
                }
                let line = lines[index];
                if let Some(rest) = line.strip_prefix(' ') {
                    old.push_str(rest);
                    new.push_str(rest);
                } else if let Some(rest) = line.strip_prefix('-') {
                    old.push_str(rest);
                } else if let Some(rest) = line.strip_prefix('+') {
                    new.push_str(rest);
                } else if next == r"\ No newline at end of file" {
                    // Informational marker emitted by unified diff.
                } else if next.is_empty()
                    && lines.get(index + 1).is_some_and(|following| {
                        following.starts_with([' ', '-', '+'])
                            || trim_line(following) == r"\ No newline at end of file"
                    })
                {
                    // Be tolerant of a common generated-diff mistake: a blank context
                    // line written as "\n" instead of the required " \n".
                    old.push_str(line);
                    new.push_str(line);
                } else {
                    return Err(format!(
                        "file {path} patch line {} is invalid inside a hunk: {next:?}; \
                         hunk lines must start with space, '-' or '+'",
                        index + 1
                    ));
                }
                saw_line = true;
                index += 1;
            }
            if !saw_line {
                return Err(format!("file {path} has an empty hunk"));
            }
            hunks.push(PatchHunk { old, new });
        }

        if hunks.is_empty() {
            return Err(format!("file {path} must contain at least one hunk"));
        }
        match (old_path, new_path) {
            (None, Some(_)) => {
                let content = hunks.into_iter().map(|hunk| hunk.new).collect();
                ops.push(ParsedPatchOp::Add { path, content });
            }
            (Some(_), None) => ops.push(ParsedPatchOp::Delete { path }),
            (Some(_), Some(_)) => ops.push(ParsedPatchOp::Update { path, hunks }),
            (None, None) => unreachable!("handled above"),
        }
    }

    if ops.is_empty() {
        return Err("patch contains no file changes".to_string());
    }
    Ok(ops)
}

fn parse_unified_path(path: &str) -> Result<Option<String>, String> {
    let path = path
        .split_whitespace()
        .next()
        .ok_or_else(|| "patch file path must not be empty".to_string())?;
    if path == "/dev/null" {
        return Ok(None);
    }
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    if path.is_empty() {
        return Err("patch file path must not be empty".to_string());
    }
    Ok(Some(path.to_string()))
}

fn trim_line(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn is_unified_patch_metadata_line(line: &str) -> bool {
    line.starts_with("index ")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("old mode ")
        || line.starts_with("new mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("dissimilarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
}

// ── RootedFsCreateTool ────────────────────────────────────────────────────────

pub struct RootedFsCreateTool {
    pub sandbox: Arc<dyn Sandbox>,
}

impl Tool for RootedFsCreateTool {
    fn name(&self) -> &str {
        "fs_mkdir"
    }
    fn description(&self) -> &str {
        "Create a directory in the workspace. Set recursive=true for mkdir -p behaviour."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        let path_description = workspace_path_description(self.sandbox.as_ref(), "Directory path");
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": path_description },
                "recursive": { "type": "boolean", "description": "Create parent dirs (default false)" }
            },
            "required": ["path"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: ToolContext,
    ) -> impl std::future::Future<
        Output = Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
    > {
        let sandbox = Arc::clone(&self.sandbox);
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_mkdir", "missing 'path'"))?
                .to_string();
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    recursive,
                    sandbox_kind = %sandbox.kind(),
                    "fs_mkdir.start"
                );
                match sandbox.mkdir(&path_str, recursive).await {
                    Ok(()) => {
                        tracing::info!(
                            path = %path_str,
                            recursive,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_mkdir.completed"
                        );
                        yield ToolOutput::text(format!("created {path_str}"));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            recursive,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_mkdir.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                }
            }))
        }
    }
}

// ── RootedFsRemoveTool ────────────────────────────────────────────────────────

pub struct RootedFsRemoveTool {
    pub sandbox: Arc<dyn Sandbox>,
}

impl Tool for RootedFsRemoveTool {
    fn name(&self) -> &str {
        "fs_remove"
    }
    fn description(&self) -> &str {
        "Remove a file or directory in the workspace. Set recursive=true to remove a directory tree."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        let path_description = workspace_path_description(self.sandbox.as_ref(), "Path to remove");
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": path_description },
                "recursive": { "type": "boolean", "description": "Remove directory recursively (default false)" }
            },
            "required": ["path"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: ToolContext,
    ) -> impl std::future::Future<
        Output = Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
    > {
        let sandbox = Arc::clone(&self.sandbox);
        async move {
            let path_str = arguments["path"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_remove", "missing 'path'"))?
                .to_string();
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    recursive,
                    sandbox_kind = %sandbox.kind(),
                    "fs_remove.start"
                );
                match sandbox.remove(&path_str, recursive).await {
                    Ok(()) => {
                        tracing::info!(
                            path = %path_str,
                            recursive,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_remove.completed"
                        );
                        yield ToolOutput::text(format!("removed {path_str}"));
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            recursive,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_remove.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                }
            }))
        }
    }
}

// ── RootedFsLsTool ────────────────────────────────────────────────────────────

pub struct RootedFsLsTool {
    pub sandbox: Arc<dyn Sandbox>,
    pub redactor: SharedRedactor,
}

impl Tool for RootedFsLsTool {
    fn name(&self) -> &str {
        "fs_ls"
    }
    fn description(&self) -> &str {
        "List the contents of a directory in the workspace."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        let path_description = format!(
            "{} Defaults to '.'.",
            workspace_path_description(self.sandbox.as_ref(), "Directory path")
        );
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": path_description }
            }
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: ToolContext,
    ) -> impl std::future::Future<
        Output = Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
    > {
        let sandbox = Arc::clone(&self.sandbox);
        let redactor = Arc::clone(&self.redactor);
        async move {
            let path_str = arguments["path"].as_str().unwrap_or(".").to_string();
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    path = %path_str,
                    sandbox_kind = %sandbox.kind(),
                    "fs_ls.start"
                );
                match sandbox.list(&path_str).await {
                    Err(e) => {
                        tracing::warn!(
                            path = %path_str,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "fs_ls.failed"
                        );
                        yield ToolOutput::text(format!("error: {e:#}"));
                    }
                    Ok(entries) => {
                        tracing::info!(
                            path = %path_str,
                            entries = entries.len(),
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "fs_ls.completed"
                        );
                        let listing = entries.join("\n");
                        let listing = redactor.read().unwrap().redact(&listing);
                        yield ToolOutput::text(listing);
                    }
                }
            }))
        }
    }
}

// ── RipgrepTool ───────────────────────────────────────────────────────────────

pub struct RipgrepTool {
    pub sandbox: Arc<dyn Sandbox>,
    pub redactor: SharedRedactor,
}

impl Tool for RipgrepTool {
    fn name(&self) -> &str {
        "rg"
    }
    fn description(&self) -> &str {
        "Search workspace files with ripgrep. Prefer this for text/code search. \
         Prefer structured args like {\"args\":[\"TODO\",\"src\",\"-g\",\"*.rs\"]}; \
         query is kept for shell-style compatibility. Do not shell-quote args entries. \
         Use -F for literal text so regex metacharacters like parentheses do not need regex escaping. \
         Paths are workspace-relative. Results include file paths, line numbers, and columns."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Preferred. Exact rg argv without the rg binary, for example [\"TODO\", \"src\", \"-g\", \"*.rs\"] or [\"-F\", \"info!(\\\"ready\\\")\", \"src\"]. Do not shell-quote entries; use separate array elements. Use -F for literal text to avoid regex escaping."
                },
                "query": { "type": "string", "description": "Compatibility fallback. Arguments to pass to rg, for example \"TODO src -g '*.rs'\". Parsed shell-style, then executed as rg argv." },
                "max_bytes": {
                    "type": "integer",
                    "description": format!("Maximum output bytes to return (default {DEFAULT_FS_READ_LENGTH}).")
                }
            },
            "anyOf": [
                { "required": ["args"] },
                { "required": ["query"] }
            ]
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
        let cancel = Some(ctx.runtime().cancellation());
        async move {
            let rg_args = rg_args_from_arguments(&arguments)?;
            let max_bytes = arguments["max_bytes"]
                .as_u64()
                .unwrap_or(DEFAULT_FS_READ_LENGTH as u64)
                .clamp(1, 256 * 1024) as usize;
            let command = rg_command(&rg_args);
            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                let command_preview = log_preview(&command, 160);
                tracing::info!(
                    command = %command_preview,
                    input_len = arguments.to_string().len(),
                    args = rg_args.len(),
                    sandbox_kind = %sandbox.kind(),
                    "rg.start"
                );
                match sandbox.bash(&command, None, 30_000, cancel).await {
                    Err(err) => {
                        tracing::warn!(
                            command = %command_preview,
                            sandbox_kind = %sandbox.kind(),
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %err,
                            "rg.failed"
                        );
                        yield ToolOutput::text(format!("error: {err:#}"));
                    }
                    Ok(output) => {
                        let stdout_bytes = output.stdout.len();
                        let stderr_bytes = output.stderr.len();
                        tracing::info!(
                            command = %command_preview,
                            sandbox_kind = %sandbox.kind(),
                            exit_code = output.exit_code,
                            stdout_bytes,
                            stderr_bytes,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "rg.completed"
                        );
                        let text = format_rg_output(output, max_bytes, &redactor);
                        yield ToolOutput::text(text);
                    }
                }
            }))
        }
    }
}

fn rg_args_from_arguments(arguments: &serde_json::Value) -> Result<Vec<String>, AgentError> {
    let args = if let Some(values) = arguments.get("args").and_then(|value| value.as_array()) {
        let mut args = Vec::new();
        for value in values {
            let arg = value
                .as_str()
                .ok_or_else(|| AgentError::tool("rg", "'args' entries must be strings"))?
                .trim();
            if !arg.is_empty() {
                args.push(arg.to_string());
            }
        }
        args
    } else {
        let query = arguments["query"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AgentError::tool("rg", "missing 'args' or 'query'"))?;
        parse_rg_query(query)?
    };
    normalize_rg_args(args)
}

fn parse_rg_query(query: &str) -> Result<Vec<String>, AgentError> {
    normalize_rg_args(
        split_rg_query_preserving_regex_escapes(query)
            .ok_or_else(|| AgentError::tool("rg", "failed to parse query; check shell quoting"))?,
    )
}

fn normalize_rg_args(mut args: Vec<String>) -> Result<Vec<String>, AgentError> {
    if matches!(args.first().map(String::as_str), Some("rg" | "ripgrep")) {
        args.remove(0);
    }
    if args.is_empty() {
        return Err(AgentError::tool("rg", "missing ripgrep arguments"));
    }
    Ok(args)
}

fn split_rg_query_preserving_regex_escapes(query: &str) -> Option<Vec<String>> {
    #[derive(Copy, Clone, Eq, PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut chars = query.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Quote::None => match ch {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => {
                    if let Some(next) = chars.peek().copied() {
                        if current.is_empty() && matches!(next, '\'' | '"') {
                            quote = if next == '\'' {
                                Quote::Single
                            } else {
                                Quote::Double
                            };
                            chars.next();
                        } else if next.is_whitespace() || matches!(next, '\'' | '"' | '\\') {
                            current.push(next);
                            chars.next();
                        } else {
                            current.push(ch);
                        }
                    } else {
                        current.push(ch);
                    }
                }
                ch if ch.is_whitespace() => {
                    if !current.is_empty() {
                        args.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(ch),
            },
            Quote::Single => match ch {
                '\'' => quote = Quote::None,
                _ => current.push(ch),
            },
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '\\' => {
                    if let Some(next) = chars.peek().copied() {
                        if next == '"' {
                            chars.next();
                            if chars.peek().is_none_or(|after| after.is_whitespace()) {
                                quote = Quote::None;
                            } else {
                                current.push(next);
                            }
                        } else if next == '\\' {
                            current.push(next);
                            chars.next();
                        } else {
                            current.push(ch);
                        }
                    } else {
                        current.push(ch);
                    }
                }
                _ => current.push(ch),
            },
        }
    }

    if quote != Quote::None {
        return None;
    }
    if !current.is_empty() {
        args.push(current);
    }
    Some(args)
}

fn rg_command(query_args: &[String]) -> String {
    let mut args = vec![
        "rg".to_string(),
        "--line-number".to_string(),
        "--column".to_string(),
        "--color".to_string(),
        "never".to_string(),
    ];
    args.extend(query_args.iter().cloned());
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn format_rg_output(
    output: SandboxBashOutput,
    max_bytes: usize,
    redactor: &SharedRedactor,
) -> String {
    if output.exit_code == 1 && output.stdout.is_empty() && output.stderr.is_empty() {
        return "No matches.".to_string();
    }
    let mut text = output.stdout;
    if !output.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr] ");
        text.push_str(&output.stderr);
    }
    if output.exit_code != 0 && output.exit_code != 1 {
        text.push_str(&format!("\n[exit {}]", output.exit_code));
    }
    let mut bytes = text.into_bytes();
    let truncated = bytes.len() > max_bytes;
    if truncated {
        bytes.truncate(max_bytes);
        while std::str::from_utf8(&bytes).is_err() {
            bytes.pop();
        }
    }
    let mut text = String::from_utf8(bytes).unwrap_or_default();
    if truncated {
        text.push_str(&format!("\n[truncated to {max_bytes} bytes]"));
    }
    redactor.read().unwrap().redact(&text)
}

// ── ExaSearchTool ─────────────────────────────────────────────────────────────

const EXA_API_URL: &str = "https://api.exa.ai/search";

pub struct ExaSearchTool {
    num_results: usize,
}

impl ExaSearchTool {
    pub fn new() -> Self {
        Self { num_results: 5 }
    }
}

impl Tool for ExaSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web via Exa. Returns titles, URLs, and content highlights. \
         Use for current events, documentation, or any topic needing fresh data. \
         Requires EXA_API_KEY secret to be set."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query":       { "type": "string",  "description": "Search query" },
                "num_results": { "type": "integer", "description": "Max results (default 5)" }
            },
            "required": ["query"]
        })
    }
    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: ToolContext,
    ) -> impl std::future::Future<
        Output = Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
    > {
        let default_n = self.num_results;
        async move {
            let api_key = match std::env::var("EXA_API_KEY") {
                Ok(k) if !k.is_empty() => k,
                _ => return Err(AgentError::tool(
                    "web_search",
                    "EXA_API_KEY is not set — use `/secrets set EXA_API_KEY <key>` to enable web search",
                )),
            };
            let query = arguments["query"]
                .as_str()
                .ok_or_else(|| AgentError::tool("web_search", "missing 'query'"))?
                .to_string();
            let num = arguments["num_results"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(default_n);

            Ok(ToolResult::Output(stream! {
                let started = Instant::now();
                tracing::info!(
                    query = %log_preview(&query, 160),
                    query_len = query.len(),
                    num_results = num,
                    "web_search.start"
                );
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .unwrap_or_else(|_| reqwest::Client::new());
                let body = serde_json::json!({
                    "query": query,
                    "numResults": num,
                    "contents": { "text": { "maxCharacters": 1000 } }
                });
                match client
                    .post(EXA_API_URL)
                    .header("x-api-key", &api_key)
                    .json(&body)
                    .send()
                    .await
                {
                    Err(e) => {
                        tracing::warn!(
                            query = %log_preview(&query, 160),
                            query_len = query.len(),
                            num_results = num,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            error = %e,
                            "web_search.failed"
                        );
                        yield ToolOutput::text(format!("search request failed: {e}"));
                    }
                    Ok(resp) => {
                        match resp.json::<serde_json::Value>().await {
                            Err(e) => {
                                tracing::warn!(
                                    query = %log_preview(&query, 160),
                                    query_len = query.len(),
                                    num_results = num,
                                    elapsed_ms = started.elapsed().as_millis() as u64,
                                    error = %e,
                                    "web_search.failed"
                                );
                                yield ToolOutput::text(format!("parse error: {e}"));
                            }
                            Ok(data) => {
                                let mut out = String::new();
                                if let Some(results) = data["results"].as_array() {
                                    tracing::info!(
                                        query = %log_preview(&query, 160),
                                        query_len = query.len(),
                                        num_results = num,
                                        results = results.len(),
                                        elapsed_ms = started.elapsed().as_millis() as u64,
                                        "web_search.completed"
                                    );
                                    for (i, r) in results.iter().enumerate() {
                                        let title = r["title"].as_str().unwrap_or("(no title)");
                                        let url   = r["url"].as_str().unwrap_or("");
                                        let text  = r["text"].as_str().unwrap_or("");
                                        out.push_str(&format!("{}. **{}**\n   {}\n   {}\n\n", i+1, title, url, text.trim()));
                                    }
                                } else {
                                    tracing::info!(
                                        query = %log_preview(&query, 160),
                                        query_len = query.len(),
                                        num_results = num,
                                        results = 0,
                                        elapsed_ms = started.elapsed().as_millis() as u64,
                                        "web_search.completed"
                                    );
                                }
                                if out.is_empty() { out = "No results found.".into(); }
                                yield ToolOutput::text(out.trim().to_string());
                            }
                        }
                    }
                }
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_command_output, format_utc_offset, parse_apply_patch, parse_manage_yourself_command,
        parse_rg_query, parse_ssh_target, parse_timezone_spec, rg_args_from_arguments, rg_command,
        ssh_command_args, validate_ssh_named, NowTool, ParsedPatchOp, PatchHunk, RipgrepTool,
        RootedFsApplyPatchTool, RootedFsReadTool, SecretRedactor, SshTarget, WorkspaceBashTool,
        WorkspaceSshTool,
    };
    use crate::sandbox::{DockerSandbox, DockerSandboxConfig, NoSandbox};
    use bot_runtime_core::ToolContext;
    use futures::StreamExt;
    use remi_agentloop::prelude::{Content, Tool, ToolOutput, ToolResult};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    const ONE_BY_ONE_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!("remi-fs-read-test-{}", uuid::Uuid::new_v4()))
    }

    fn test_tool_context() -> ToolContext {
        ToolContext::with_ids(
            serde_json::from_value(serde_json::json!("test-thread"))
                .expect("thread_id should deserialize"),
            serde_json::from_value(serde_json::json!("test-run"))
                .expect("run_id should deserialize"),
            bot_runtime_core::ChatCtxState::default(),
        )
    }

    #[test]
    fn now_timezone_parser_accepts_offsets_and_aliases() {
        assert_eq!(parse_timezone_spec("UTC").unwrap().offset_seconds, 0);
        assert_eq!(
            parse_timezone_spec("Asia/Shanghai").unwrap().offset_seconds,
            8 * 3600
        );
        assert_eq!(
            parse_timezone_spec("+08:00").unwrap().offset_seconds,
            8 * 3600
        );
        assert_eq!(
            parse_timezone_spec("-0530").unwrap().offset_seconds,
            -19_800
        );
        assert_eq!(format_utc_offset(-19_800), "-05:30");
        assert!(parse_timezone_spec("not-a-zone").is_none());
    }

    #[tokio::test]
    async fn now_tool_returns_timezone_timestamp_and_weekday() {
        let content = collect_tool_content(
            <NowTool as Tool>::execute(
                &NowTool,
                json!({ "timezone": "Asia/Shanghai" }),
                None,
                test_tool_context(),
            )
            .await
            .expect("now should return tool output"),
        )
        .await;
        let value: serde_json::Value =
            serde_json::from_str(&content.text_content()).expect("now output should be json");

        assert_eq!(value["timezone"], "Asia/Shanghai");
        assert_eq!(value["utc_offset"], "+08:00");
        assert!(value["datetime"].as_str().unwrap().contains("+08:00"));
        assert!(value["unix_timestamp"].as_u64().is_some());
        assert!(value["unix_timestamp_ms"].as_u64().is_some());
        assert!(value["weekday"]["english"].as_str().is_some());
        assert!(value["weekday"]["chinese"].as_str().is_some());
        assert!(value["weekday"]["iso_number"].as_u64().is_some());
    }

    #[test]
    fn manage_yourself_parses_shell_like_command_without_shell() {
        let args =
            parse_manage_yourself_command("profile agent upsert dev './agents/coder profile.md'")
                .expect("quoted command should parse");

        assert_eq!(
            args,
            vec![
                "profile",
                "agent",
                "upsert",
                "dev",
                "./agents/coder profile.md"
            ]
        );
    }

    #[test]
    fn manage_yourself_rejects_empty_or_unclosed_quotes() {
        assert!(parse_manage_yourself_command("   ").is_err());
        assert!(parse_manage_yourself_command("profile 'unterminated").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn manage_yourself_formats_nonzero_exit_output() {
        use std::os::unix::process::ExitStatusExt;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(2 << 8),
            stdout: b"stdout\n".to_vec(),
            stderr: b"stderr\n".to_vec(),
        };

        assert_eq!(
            format_command_output(output),
            "stdout\n[stderr] stderr\n[exit 2]"
        );
    }

    async fn collect_tool_content(
        result: ToolResult<impl futures::Stream<Item = ToolOutput>>,
    ) -> Content {
        match result {
            ToolResult::Interrupt(_) => Content::text("interrupted"),
            ToolResult::Output(output) => {
                let mut output = std::pin::pin!(output);
                let mut last = Content::text(String::new());
                while let Some(item) = output.next().await {
                    if let ToolOutput::Result(content) = item {
                        last = content;
                    }
                }
                last
            }
        }
    }

    async fn collect_tool_text(
        result: ToolResult<impl futures::Stream<Item = ToolOutput>>,
    ) -> String {
        collect_tool_content(result).await.text_content()
    }

    #[test]
    fn bash_and_ssh_schemas_do_not_expose_legacy_task_fields() {
        let bash = WorkspaceBashTool::new(
            Arc::new(NoSandbox::new(test_root())),
            Arc::new(RwLock::new(SecretRedactor::empty())),
        );
        let ssh = WorkspaceSshTool::new(Arc::new(RwLock::new(SecretRedactor::empty())));

        for schema in [
            <WorkspaceBashTool as Tool>::parameters_schema(&bash),
            <WorkspaceSshTool as Tool>::parameters_schema(&ssh),
        ] {
            let properties = schema["properties"]
                .as_object()
                .expect("tool schema should contain properties");
            for legacy in ["timeout_ms", "pid", "action", "running"] {
                assert!(
                    !properties.contains_key(legacy),
                    "{legacy} should be managed by the background task layer, not tool schema"
                );
            }
        }
    }

    #[test]
    fn rg_command_quotes_arguments() {
        let args = parse_rg_query(r#""can't touch this" src/lib.rs -g '*.rs' -t rust"#)
            .expect("query should parse");
        let command = rg_command(&args);
        assert!(command.contains("'can'\"'\"'t touch this'"));
        assert!(command.contains("'-g' '*.rs'"));
        assert!(command.contains("'-t' 'rust'"));
        assert!(command.ends_with("'-t' 'rust'"));
    }

    #[test]
    fn rg_query_preserves_regex_escapes() {
        let args = parse_rg_query(r#"info!\( src/ --max-count 10"#).expect("query should parse");
        assert_eq!(args[0], r#"info!\("#);

        let command = rg_command(&args);
        assert!(command.contains(r#"'info!\('"#));
    }

    #[test]
    fn rg_query_still_supports_shell_style_quotes() {
        let args =
            parse_rg_query(r#"alpha\ beta "quoted value" 'glob*.rs'"#).expect("query should parse");
        assert_eq!(args, vec!["alpha beta", "quoted value", "glob*.rs"]);
    }

    #[test]
    fn rg_query_accepts_json_style_escaped_quotes() {
        let args =
            parse_rg_query(r#"alpha \"src/lib.rs\" -g \"*.rs\""#).expect("query should parse");
        assert_eq!(args, vec!["alpha", "src/lib.rs", "-g", "*.rs"]);
    }

    #[test]
    fn rg_query_strips_accidental_binary_prefix() {
        let args = parse_rg_query(r#"rg alpha src -g '*.rs'"#).expect("query should parse");
        assert_eq!(args, vec!["alpha", "src", "-g", "*.rs"]);
    }

    #[test]
    fn rg_args_accept_structured_complex_values() {
        let args = rg_args_from_arguments(&json!({
            "args": [
                r#"info!\(\"ready\"\)"#,
                "src/with spaces",
                "-g",
                "*.{rs,md}",
                "--max-count",
                "10"
            ]
        }))
        .expect("structured args should parse");

        assert_eq!(
            args,
            vec![
                r#"info!\(\"ready\"\)"#,
                "src/with spaces",
                "-g",
                "*.{rs,md}",
                "--max-count",
                "10"
            ]
        );
    }

    #[tokio::test]
    async fn rg_tool_searches_workspace() {
        let root = test_root();
        tokio::fs::create_dir_all(root.join("src"))
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("src/lib.rs"), "fn alpha() {}\nfn beta() {}\n")
            .await
            .unwrap();
        tokio::fs::write(root.join("README.md"), "alpha docs\n")
            .await
            .unwrap();
        let tool = RipgrepTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let output = collect_tool_text(
            <RipgrepTool as Tool>::execute(
                &tool,
                json!({
                    "query": "alpha src -g '*.rs'"
                }),
                None,
                ctx.clone(),
            )
            .await
            .expect("rg should execute"),
        )
        .await;

        assert!(output.contains("src/lib.rs:1:4:"));
        assert!(output.contains("fn alpha() {}"));
        assert!(!output.contains("README.md"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn rg_tool_accepts_json_style_escaped_path_quotes() {
        let root = test_root();
        tokio::fs::create_dir_all(root.join("src"))
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("src/lib.rs"), "fn alpha() {}\n")
            .await
            .unwrap();
        let tool = RipgrepTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let output = collect_tool_text(
            <RipgrepTool as Tool>::execute(
                &tool,
                json!({
                    "query": r#"alpha \"src/lib.rs\""#
                }),
                None,
                ctx.clone(),
            )
            .await
            .expect("rg should execute"),
        )
        .await;

        assert!(output.contains("1:4:"));
        assert!(output.contains("fn alpha() {}"));
        assert!(!output.contains("No such file"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn rg_tool_accepts_structured_complex_args() {
        let root = test_root();
        tokio::fs::create_dir_all(root.join("src/with spaces"))
            .await
            .expect("test root should be created");
        tokio::fs::write(
            root.join("src/with spaces/lib.rs"),
            "info!(\"ready\");\ninfo!(\"skip\");\n",
        )
        .await
        .unwrap();
        let tool = RipgrepTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let output = collect_tool_text(
            <RipgrepTool as Tool>::execute(
                &tool,
                json!({
                    "args": [
                        r#"info!\("ready"\)"#,
                        "src/with spaces",
                        "-g",
                        "*.rs",
                        "--max-count",
                        "5"
                    ]
                }),
                None,
                ctx.clone(),
            )
            .await
            .expect("rg should execute"),
        )
        .await;

        assert!(output.contains("src/with spaces/lib.rs:1:1:"));
        assert!(output.contains("info!(\"ready\");"));
        assert!(!output.contains("No such file"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn rg_tool_accepts_fixed_literal_without_regex_escaping() {
        let root = test_root();
        tokio::fs::create_dir_all(root.join("src"))
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("src/lib.rs"), "info!(\"ready\");\n")
            .await
            .unwrap();
        let tool = RipgrepTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let output = collect_tool_text(
            <RipgrepTool as Tool>::execute(
                &tool,
                json!({
                    "args": ["-F", "info!(\"ready\")", "src"]
                }),
                None,
                ctx.clone(),
            )
            .await
            .expect("rg should execute"),
        )
        .await;

        assert!(output.contains("src/lib.rs:1:1:"));
        assert!(output.contains("info!(\"ready\");"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn rg_tool_preserves_regex_escape_in_search_pattern() {
        let root = test_root();
        tokio::fs::create_dir_all(root.join("src"))
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("src/lib.rs"), "info!(\"ready\");\n")
            .await
            .unwrap();
        let tool = RipgrepTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let output = collect_tool_text(
            <RipgrepTool as Tool>::execute(
                &tool,
                json!({
                    "query": r#"info!\( src --max-count 10"#
                }),
                None,
                ctx.clone(),
            )
            .await
            .expect("rg should execute"),
        )
        .await;

        assert!(output.contains("src/lib.rs:1:1:"));
        assert!(output.contains("info!(\"ready\");"));
        assert!(!output.contains("regex parse error"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn ssh_target_parser_validates_structured_target() {
        let target = parse_ssh_target(&json!({
            "host": "prod",
            "user": "deploy",
            "port": 2222
        }))
        .expect("valid ssh target should parse");
        assert_eq!(target.host, "prod");
        assert_eq!(target.user.as_deref(), Some("deploy"));
        assert_eq!(target.port, Some(2222));

        assert!(parse_ssh_target(&json!({ "host": "" })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "bad host" })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "-bad" })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "prod", "user": "bad user" })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "prod", "port": 0 })).is_err());
        assert!(parse_ssh_target(&json!({ "host": "prod", "port": 70000 })).is_err());
    }

    #[test]
    fn ssh_command_args_are_structured_without_local_shell() {
        let target = SshTarget {
            host: "prod".to_string(),
            user: Some("deploy".to_string()),
            port: Some(2222),
        };
        assert_eq!(
            ssh_command_args(&target, Some("printf hello; pwd")),
            vec![
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-l",
                "deploy",
                "-p",
                "2222",
                "prod",
                "printf hello; pwd"
            ]
        );

        let target = SshTarget {
            host: "prod".to_string(),
            user: None,
            port: None,
        };
        assert_eq!(
            ssh_command_args(&target, None),
            vec![
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "prod"
            ]
        );
    }

    #[test]
    fn ssh_named_session_names_are_restricted() {
        assert_eq!(validate_ssh_named("alpha_1").unwrap(), "alpha_1");
        assert!(validate_ssh_named("").is_err());
        assert!(validate_ssh_named("bad/name").is_err());
        assert!(validate_ssh_named(&"a".repeat(65)).is_err());
    }

    #[tokio::test]
    async fn fs_read_returns_multimodal_content_for_png() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("tiny.png"), ONE_BY_ONE_PNG)
            .await
            .expect("png fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "tiny.png", "offset": 5, "length": 3 }),
                None,
                test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await;

        match content {
            Content::Parts(parts) => {
                assert!(matches!(
                    parts.first(),
                    Some(remi_agentloop::prelude::ContentPart::Text { text })
                        if text.contains("mime_type=image/png")
                            && text.contains("offset/length are ignored")
                ));
                assert!(matches!(
                    parts.get(1),
                    Some(remi_agentloop::prelude::ContentPart::ImageUrl { image_url })
                        if image_url.url.starts_with("data:image/png;base64,")
                ));
            }
            other => panic!("expected multimodal content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fs_read_keeps_text_chunking_for_plain_text() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "hello world")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 6, "length": 5 }),
                None,
                test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await;

        let text = content.text_content();
        assert!(text.contains("world"));
        assert!(text.contains("[offset=6 length=5 total_bytes=11 remaining=0]"));
    }

    #[tokio::test]
    async fn fs_read_supports_inclusive_line_ranges() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "one\ntwo\nthree\nfour\nfive\n")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "start_line": 2, "end_line": 4, "offset": 99, "length": 1 }),
                None,
                test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await;

        let text = content.text_content();
        assert!(text.starts_with("two\nthree\nfour\n"));
        assert!(!text.contains("one"));
        assert!(!text.contains("five"));
        assert!(text.contains("[start_line=2 end_line=4 total_lines=5]"));
    }

    #[tokio::test]
    async fn fs_read_supports_single_sided_line_ranges() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "one\ntwo\nthree\nfour\n")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let from_start = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "end_line": 2 }),
                None,
                test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(from_start.starts_with("one\ntwo\n"));
        assert!(from_start.contains("[start_line=1 end_line=2 total_lines=4]"));

        let to_end = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "start_line": 3 }),
                None,
                test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(to_end.starts_with("three\nfour\n"));
        assert!(to_end.contains("[start_line=3 end_line=4 total_lines=4]"));
    }

    #[tokio::test]
    async fn fs_read_rejects_invalid_line_ranges() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let err = match <RootedFsReadTool as Tool>::execute(
            &tool,
            json!({ "path": "note.txt", "start_line": 3, "end_line": 2 }),
            None,
            test_tool_context(),
        )
        .await
        {
            Ok(_) => panic!("invalid line range should fail before reading"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("end_line must be greater"));

        let err = match <RootedFsReadTool as Tool>::execute(
            &tool,
            json!({ "path": "note.txt", "start_line": 0 }),
            None,
            test_tool_context(),
        )
        .await
        {
            Ok(_) => panic!("zero start_line should fail before reading"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("start_line must be >= 1"));

        let err = match <RootedFsReadTool as Tool>::execute(
            &tool,
            json!({ "path": "note.txt", "end_line": -1 }),
            None,
            test_tool_context(),
        )
        .await
        {
            Ok(_) => panic!("negative end_line should fail before reading"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("end_line must be >= 1"));
    }

    #[tokio::test]
    async fn fs_read_reports_directories_and_lists_entries() {
        let root = test_root();
        let dir = root.join("notes");
        tokio::fs::create_dir_all(dir.join("nested"))
            .await
            .expect("test directory should be created");
        tokio::fs::write(dir.join("a.txt"), "hello")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "notes" }),
                None,
                test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await
        .text_content();

        assert!(content.starts_with("notes is a directory; use fs_ls"));
        assert!(content.contains("a.txt"));
        assert!(content.contains("nested/"));
    }

    #[tokio::test]
    async fn fs_read_warns_when_repeating_same_range_unchanged() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "hello world")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let first = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                ctx.clone(),
            )
            .await
            .expect("first fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(!first.contains("repeats a range"));

        let second = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                ctx.clone(),
            )
            .await
            .expect("second fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(second.contains("repeats a range already read"));
        assert!(second.contains("Continue with offset=5"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn fs_read_different_range_clears_previous_duplicate_marker() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "hello world")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let _ = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                ctx.clone(),
            )
            .await
            .expect("first fs_read should succeed"),
        )
        .await;

        let next = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 6, "length": 5 }),
                None,
                ctx.clone(),
            )
            .await
            .expect("next fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(!next.contains("repeats a range"));
        assert!(next.contains("world"));

        let repeated_next = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 6, "length": 5 }),
                None,
                ctx.clone(),
            )
            .await
            .expect("repeated fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(repeated_next.contains("repeats a range already read"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn fs_read_same_range_after_mtime_change_does_not_warn() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        let path = root.join("note.txt");
        tokio::fs::write(&path, "hello world")
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };
        let ctx = test_tool_context();

        let _ = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                ctx.clone(),
            )
            .await
            .expect("first fs_read should succeed"),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        tokio::fs::write(&path, "HELLO world")
            .await
            .expect("text fixture should be rewritten");

        let after_change = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "note.txt", "offset": 0, "length": 5 }),
                None,
                ctx.clone(),
            )
            .await
            .expect("second fs_read should succeed"),
        )
        .await
        .text_content();
        assert!(!after_change.contains("repeats a range"));
        assert!(after_change.contains("HELLO"));
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn fs_read_default_length_handles_larger_source_chunks() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        let body = "a".repeat(12_000);
        tokio::fs::write(root.join("large.rs"), &body)
            .await
            .expect("text fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "large.rs" }),
                None,
                test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await;

        let text = content.text_content();
        assert!(text.contains("[offset=0 length=12000 total_bytes=12000 remaining=0]"));
    }

    #[tokio::test]
    async fn fs_read_keeps_non_image_binary_on_text_path() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("blob.bin"), [0x00_u8, 0x9f, 0x92, 0x96])
            .await
            .expect("binary fixture should be written");

        let tool = RootedFsReadTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
            redactor: Arc::new(RwLock::new(SecretRedactor::empty())),
        };

        let content = collect_tool_content(
            <RootedFsReadTool as Tool>::execute(
                &tool,
                json!({ "path": "blob.bin" }),
                None,
                test_tool_context(),
            )
            .await
            .expect("fs_read should succeed"),
        )
        .await;

        assert!(matches!(content, Content::Text(_)));
        assert!(content
            .text_content()
            .contains("[offset=0 length=4 total_bytes=4 remaining=0]"));
    }

    #[tokio::test]
    async fn apply_patch_updates_adds_and_deletes_multiple_files() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("a.txt"), "alpha\nbeta\n")
            .await
            .expect("a fixture should be written");
        tokio::fs::write(root.join("b.txt"), "remove me\n")
            .await
            .expect("b fixture should be written");

        let tool = RootedFsApplyPatchTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
        };
        let patch = r#"diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,2 +1,2 @@
-alpha
+ALPHA
 beta
diff --git a/c.txt b/c.txt
new file mode 100644
--- /dev/null
+++ b/c.txt
@@ -0,0 +1 @@
+created
diff --git a/b.txt b/b.txt
deleted file mode 100644
--- a/b.txt
+++ /dev/null
@@ -1 +0,0 @@
-remove me
"#;

        let content = collect_tool_content(
            <RootedFsApplyPatchTool as Tool>::execute(
                &tool,
                json!({ "patch": patch }),
                None,
                test_tool_context(),
            )
            .await
            .expect("apply_patch should succeed"),
        )
        .await;

        assert!(content.text_content().contains("updated 1 file(s): a.txt"));
        assert_eq!(
            tokio::fs::read_to_string(root.join("a.txt")).await.unwrap(),
            "ALPHA\nbeta\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(root.join("c.txt")).await.unwrap(),
            "created\n"
        );
        assert!(!root.join("b.txt").exists());
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn apply_patch_accepts_unprefixed_blank_context_lines() {
        let patch = r#"--- a/note.txt
+++ b/note.txt
@@ -1,3 +1,3 @@
-alpha
+ALPHA

 omega
"#;

        let ops = parse_apply_patch(patch).expect("blank context line should be tolerated");

        assert_eq!(
            ops,
            vec![ParsedPatchOp::Update {
                path: "note.txt".to_string(),
                hunks: vec![PatchHunk {
                    old: "alpha\n\nomega\n".to_string(),
                    new: "ALPHA\n\nomega\n".to_string(),
                }],
            }]
        );
    }

    #[test]
    fn apply_patch_reports_invalid_hunk_line_number_and_content() {
        let patch = r#"--- a/note.txt
+++ b/note.txt
@@ -1 +1 @@
 alpha
invalid
"#;

        let error = parse_apply_patch(patch).expect_err("invalid hunk line should fail");

        assert!(error.contains("patch line 5"), "{error}");
        assert!(error.contains("\"invalid\""), "{error}");
    }

    #[tokio::test]
    async fn apply_patch_rejects_ambiguous_update_hunks() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("note.txt"), "same\nsame\n")
            .await
            .expect("fixture should be written");

        let tool = RootedFsApplyPatchTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
        };
        let patch = r#"diff --git a/note.txt b/note.txt
--- a/note.txt
+++ b/note.txt
@@ -1 +1 @@
-same
+changed
"#;

        let content = collect_tool_content(
            <RootedFsApplyPatchTool as Tool>::execute(
                &tool,
                json!({ "patch": patch }),
                None,
                test_tool_context(),
            )
            .await
            .expect("apply_patch should return tool output"),
        )
        .await;

        assert!(content.text_content().contains("matched 2 times"));
        assert_eq!(
            tokio::fs::read_to_string(root.join("note.txt"))
                .await
                .unwrap(),
            "same\nsame\n"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn apply_patch_supports_standard_diff_relative_to_workdir() {
        let root = test_root();
        tokio::fs::create_dir_all(root.join("repo"))
            .await
            .expect("test repo should be created");
        tokio::fs::write(root.join("repo/app.py"), "print('old')\n")
            .await
            .expect("fixture should be written");

        let tool = RootedFsApplyPatchTool {
            sandbox: Arc::new(NoSandbox::new(root.clone())),
        };
        let patch = r#"diff --git a/app.py b/app.py
--- a/app.py
+++ b/app.py
@@ -1 +1 @@
-print('old')
+print('new')
"#;

        let content = collect_tool_content(
            <RootedFsApplyPatchTool as Tool>::execute(
                &tool,
                json!({ "patch": patch, "workdir": "repo" }),
                None,
                test_tool_context(),
            )
            .await
            .expect("apply_patch should succeed"),
        )
        .await;

        assert!(content
            .text_content()
            .contains("updated 1 file(s): repo/app.py"));
        assert_eq!(
            tokio::fs::read_to_string(root.join("repo/app.py"))
                .await
                .unwrap(),
            "print('new')\n"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn apply_patch_accepts_docker_workspace_absolute_paths() {
        let root = test_root();
        tokio::fs::create_dir_all(&root)
            .await
            .expect("test root should be created");
        tokio::fs::write(root.join("app.py"), "print('old')\n")
            .await
            .expect("fixture should be written");

        let tool = RootedFsApplyPatchTool {
            sandbox: Arc::new(DockerSandbox::new(DockerSandboxConfig {
                host_dir: root.clone(),
                container_dir: "/workspace".to_string(),
                image: "unused".to_string(),
                container_name: "unused".to_string(),
                user: None,
            })),
        };
        let patch = r#"diff --git a/app.py b/app.py
--- /workspace/app.py
+++ /workspace/app.py
@@ -1 +1 @@
-print('old')
+print('new')
"#;

        let content = collect_tool_content(
            <RootedFsApplyPatchTool as Tool>::execute(
                &tool,
                json!({ "patch": patch }),
                None,
                test_tool_context(),
            )
            .await
            .expect("apply_patch should succeed"),
        )
        .await;

        assert!(content
            .text_content()
            .contains("updated 1 file(s): /workspace/app.py"));
        assert_eq!(
            tokio::fs::read_to_string(root.join("app.py"))
                .await
                .unwrap(),
            "print('new')\n"
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
