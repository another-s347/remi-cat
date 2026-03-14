//! Workspace-rooted filesystem tools, bash executor, and web search.
//!
//! All `Rooted*` tools sandbox every path under a root directory so absolute
//! paths like `/etc/passwd` map to `<root>/etc/passwd`.
//!
//! `WorkspaceBashTool` runs real `bash -c` with `current_dir` set to the root.
//!
//! `ExaSearchTool` calls the Exa Search API when `EXA_API_KEY` is set.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use aho_corasick::{AhoCorasick, MatchKind};
use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{
    AgentError, ResumePayload, Tool, ToolContext, ToolOutput, ToolResult,
};

// ── helper ────────────────────────────────────────────────────────────────────

fn resolve(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

// ── SecretRedactor ────────────────────────────────────────────────────────────

/// Redacts secret values from tool output text using Aho-Corasick multi-pattern
/// search.  Thread-safe; intended to be held behind `Arc<RwLock<_>>` and rebuilt
/// when the secret set changes.
pub struct SecretRedactor {
    /// Built automaton, `None` when there are no secrets.
    ac: Option<AhoCorasick>,
    /// Replacement labels — index-aligned with the AhoCorasick patterns.
    labels: Vec<String>,
}

impl SecretRedactor {
    /// Construct a redactor with no patterns (identity transform).
    pub fn empty() -> Self {
        Self {
            ac: None,
            labels: Vec::new(),
        }
    }

    /// (Re-)build from a `key → value` map.
    ///
    /// Only non-empty values are included; values shorter than 4 bytes are
    /// skipped to avoid spurious matches of very short strings.
    pub fn from_entries(entries: &HashMap<String, String>) -> Self {
        let mut patterns: Vec<&str> = Vec::new();
        let mut labels: Vec<String> = Vec::new();

        for (key, val) in entries {
            if val.len() < 4 {
                continue;
            }
            patterns.push(val);
            labels.push(key.clone());
        }

        if patterns.is_empty() {
            return Self::empty();
        }

        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(patterns)
            .expect("AhoCorasick build failed");

        Self {
            ac: Some(ac),
            labels,
        }
    }

    /// Replace all secret values in `text` with `[REDACTED:<KEY>]`.
    /// Returns the original string unchanged when no secrets are configured.
    pub fn redact(&self, text: &str) -> String {
        let Some(ac) = &self.ac else {
            return text.to_string();
        };

        let mut result = String::with_capacity(text.len());
        let mut last = 0usize;

        for m in ac.find_iter(text) {
            result.push_str(&text[last..m.start()]);
            result.push_str("[REDACTED:");
            result.push_str(&self.labels[m.pattern().as_usize()]);
            result.push(']');
            last = m.end();
        }
        result.push_str(&text[last..]);
        result
    }
}

/// Convenience alias used by `CatBot`.
pub type SharedRedactor = Arc<RwLock<SecretRedactor>>;

// ── BashMode ──────────────────────────────────────────────────────────────────

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
    pub root: PathBuf,
    pub redactor: SharedRedactor,
    pub mode: BashMode,
}

impl WorkspaceBashTool {
    pub fn new(root: impl Into<PathBuf>, redactor: SharedRedactor, mode: BashMode) -> Self {
        Self {
            root: root.into(),
            redactor,
            mode,
        }
    }
}

impl Tool for WorkspaceBashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        match self.mode {
            BashMode::Local => {
                "Execute a bash command in the agent workspace (local mode). \
                 Working directory is the agent data folder; relative paths resolve there. \
                 ⚠️ Each invocation is a fresh one-time session — no state \
                 (variables, directory changes, background processes) persists \
                 between calls. Write results to files if you need them later."
            }
            BashMode::Docker => {
                "Execute a bash command in the agent workspace. \
                 Working directory is the agent workspace root (contains soul.md, memory/, etc.). \
                 ⚠️ Each invocation is a fresh one-time session — no state \
                 (variables, directory changes, background processes) persists \
                 between calls. Write results to files if you need them later."
            }
        }
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command":    { "type": "string",  "description": "Shell command to execute" },
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
        let redactor = Arc::clone(&self.redactor);
        let mode = self.mode;
        async move {
            let command = arguments["command"]
                .as_str()
                .ok_or_else(|| AgentError::tool("bash", "missing 'command'"))?
                .to_string();
            let timeout_ms = arguments["timeout_ms"].as_u64().unwrap_or(30_000);
            Ok(ToolResult::Output(stream! {
                yield ToolOutput::Delta(format!("$ {}", command));
                tracing::debug!(cmd = %command, timeout_ms, ?mode, "bash: running");
                let mut cmd = tokio::process::Command::new("bash");
                cmd.arg("-c").arg(&command);
                let _ = std::fs::create_dir_all(&root);
                cmd.current_dir(&root);
                let run = cmd.output();
                match tokio::time::timeout(Duration::from_millis(timeout_ms), run).await {
                    Err(_) => {
                        tracing::warn!(cmd = %command, timeout_ms, "bash: timed out");
                        yield ToolOutput::text(format!("[timed out after {timeout_ms}ms]"));
                    }
                    Ok(Err(e)) => yield ToolOutput::text(format!("error: {e}")),
                    Ok(Ok(o)) => {
                        let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                        let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
                        let code   = o.status.code().unwrap_or(-1);
                        tracing::debug!(cmd = %command, code, "bash: done");
                        let mut r = stdout;
                        if !stderr.is_empty() {
                            if !r.is_empty() { r.push('\n'); }
                            r.push_str("[stderr] "); r.push_str(&stderr);
                        }
                        if code != 0 { r.push_str(&format!("\n[exit {code}]")); }
                        let r = redactor.read().unwrap().redact(&r);
                        yield ToolOutput::text(r);
                    }
                }
            }))
        }
    }
}

// ── RootedFsReadTool ──────────────────────────────────────────────────────────

pub struct RootedFsReadTool {
    pub root: PathBuf,
    pub redactor: SharedRedactor,
}

impl Tool for RootedFsReadTool {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn description(&self) -> &str {
        "Read a file in the workspace. Supports `offset` + `length` for chunked reading. \
         Always check `[total_bytes]` in the result and call again with offset += length if needed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string",  "description": "File path (relative to workspace root)" },
                "offset": { "type": "integer", "description": "Byte offset to start (default 0)" },
                "length": { "type": "integer", "description": "Max bytes to return (default 8192)" }
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
        let redactor = Arc::clone(&self.redactor);
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
                    Err(e) => yield ToolOutput::text(format!("error: {e}")),
                    Ok(bytes) => {
                        let total = bytes.len();
                        let start = offset.min(total);
                        let end   = (start + length).min(total);
                        let text  = String::from_utf8_lossy(&bytes[start..end]).into_owned();
                        let remaining = total.saturating_sub(end);
                        let text = redactor.read().unwrap().redact(&text);
                        let mut r = text;
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
    pub root: PathBuf,
}

impl Tool for RootedFsWriteTool {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn description(&self) -> &str {
        "Write text to a file in the workspace. Path is relative to workspace root. \
         Parent directories must already exist."
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
                match tokio::fs::write(&full, content.as_bytes()).await {
                    Ok(()) => yield ToolOutput::text(format!("wrote {bytes} bytes to {path_str}")),
                    Err(e) => yield ToolOutput::text(format!("error: {e}")),
                }
            }))
        }
    }
}

// ── RootedFsReplaceTool ───────────────────────────────────────────────────────

pub struct RootedFsReplaceTool {
    pub root: PathBuf,
}

impl Tool for RootedFsReplaceTool {
    fn name(&self) -> &str {
        "fs_replace"
    }
    fn description(&self) -> &str {
        "Replace the first (and only) occurrence of `old` with `new` inside a file. \
         Returns an error if `old` is not found, or if it matches more than once \
         (include more surrounding context to make the match unique)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path (relative to workspace root)" },
                "old":  { "type": "string", "description": "Exact text to find (must appear exactly once)" },
                "new":  { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old", "new"]
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
                .ok_or_else(|| AgentError::tool("fs_replace", "missing 'path'"))?
                .to_string();
            let old = arguments["old"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_replace", "missing 'old'"))?
                .to_string();
            let new = arguments["new"]
                .as_str()
                .ok_or_else(|| AgentError::tool("fs_replace", "missing 'new'"))?
                .to_string();
            let full = resolve(&root, &path_str);
            Ok(ToolResult::Output(stream! {
                match tokio::fs::read_to_string(&full).await {
                    Err(e) => yield ToolOutput::text(format!("error: {e}")),
                    Ok(content) => {
                        let count = content.matches(old.as_str()).count();
                        if count == 0 {
                            yield ToolOutput::text(format!("error: 'old' string not found in {path_str}"));
                        } else if count > 1 {
                            yield ToolOutput::text(format!(
                                "error: 'old' matches {count} times in {path_str} — \
                                 add more surrounding context to make it unique"
                            ));
                        } else {
                            let replaced = content.replacen(old.as_str(), new.as_str(), 1);
                            match tokio::fs::write(&full, replaced.as_bytes()).await {
                                Ok(()) => yield ToolOutput::text(format!(
                                    "replaced 1 occurrence in {path_str}"
                                )),
                                Err(e) => yield ToolOutput::text(format!("error writing: {e}")),
                            }
                        }
                    }
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
        "Create a directory in the workspace. Set recursive=true for mkdir -p behaviour."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": "Directory path (relative to workspace root)" },
                "recursive": { "type": "boolean", "description": "Create parent dirs (default false)" }
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
            let recursive = arguments["recursive"].as_bool().unwrap_or(false);
            let full = resolve(&root, &path_str);
            Ok(ToolResult::Output(stream! {
                let result = if recursive {
                    tokio::fs::create_dir_all(&full).await
                } else {
                    tokio::fs::create_dir(&full).await
                };
                match result {
                    Ok(()) => yield ToolOutput::text(format!("created {path_str}")),
                    Err(e) => yield ToolOutput::text(format!("error: {e}")),
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
        "Remove a file or directory in the workspace. Set recursive=true to remove a directory tree."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": "Path to remove (relative to workspace root)" },
                "recursive": { "type": "boolean", "description": "Remove directory recursively (default false)" }
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
                } else if full.is_dir() {
                    tokio::fs::remove_dir(&full).await
                } else {
                    tokio::fs::remove_file(&full).await
                };
                match result {
                    Ok(()) => yield ToolOutput::text(format!("removed {path_str}")),
                    Err(e) => yield ToolOutput::text(format!("error: {e}")),
                }
            }))
        }
    }
}

// ── RootedFsLsTool ────────────────────────────────────────────────────────────

pub struct RootedFsLsTool {
    pub root: PathBuf,
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
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path (relative to workspace root, default '.')" }
            }
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
        let redactor = Arc::clone(&self.redactor);
        async move {
            let path_str = arguments["path"].as_str().unwrap_or(".").to_string();
            let full = resolve(&root, &path_str);
            Ok(ToolResult::Output(stream! {
                match tokio::fs::read_dir(&full).await {
                    Err(e) => yield ToolOutput::text(format!("error: {e}")),
                    Ok(mut rd) => {
                        let mut entries = Vec::new();
                        while let Ok(Some(entry)) = rd.next_entry().await {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            let suffix = if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) { "/" } else { "" };
                            entries.push(format!("{name}{suffix}"));
                        }
                        entries.sort();
                        let listing = entries.join("\n");
                        let listing = redactor.read().unwrap().redact(&listing);
                        yield ToolOutput::text(listing);
                    }
                }
            }))
        }
    }
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
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
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
                tracing::debug!(query = %query, "web_search: querying");
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
                        tracing::warn!("web_search failed: {e}");
                        yield ToolOutput::text(format!("search request failed: {e}"));
                    }
                    Ok(resp) => {
                        match resp.json::<serde_json::Value>().await {
                            Err(e) => { yield ToolOutput::text(format!("parse error: {e}")); }
                            Ok(data) => {
                                let mut out = String::new();
                                if let Some(results) = data["results"].as_array() {
                                    tracing::debug!(query = %query, n = results.len(), "web_search: done");
                                    for (i, r) in results.iter().enumerate() {
                                        let title = r["title"].as_str().unwrap_or("(no title)");
                                        let url   = r["url"].as_str().unwrap_or("");
                                        let text  = r["text"].as_str().unwrap_or("");
                                        out.push_str(&format!("{}. **{}**\n   {}\n   {}\n\n", i+1, title, url, text.trim()));
                                    }
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

// ── NowTool ───────────────────────────────────────────────────────────────────

/// Returns the current UTC date and time.
pub struct NowTool;

impl Tool for NowTool {
    fn name(&self) -> &str {
        "now"
    }
    fn description(&self) -> &str {
        "Return the current UTC date and time in ISO 8601 format. \
         Use this whenever you need to know what time or date it is."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }
    fn execute(
        &self,
        _arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        async move {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let secs = now.as_secs();
            // Format as ISO 8601 UTC: YYYY-MM-DDTHH:MM:SSZ
            let s = secs % 60;
            let m = (secs / 60) % 60;
            let h = (secs / 3600) % 24;
            let days = secs / 86400; // days since 1970-01-01
            let (year, month, day) = days_to_ymd(days);
            let formatted = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z (unix: {})",
                year, month, day, h, m, s, secs
            );
            Ok(ToolResult::Output(async_stream::stream! {
                yield ToolOutput::text(formatted);
            }))
        }
    }
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Shift epoch to 1 Mar 0000 (proleptic Gregorian) for easier math.
    days += 719468;
    let era = days / 146097;
    let doe = days % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
