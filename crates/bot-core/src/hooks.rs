use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::timeout;
use tracing::{debug, warn};

const DEFAULT_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookEventName {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PreCompact,
    PostCompact,
    SubagentStart,
    SubagentStop,
    Stop,
}

impl HookEventName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PostToolUse => "PostToolUse",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::Stop => "Stop",
        }
    }

    fn from_key(value: &str) -> Option<Self> {
        match value {
            "SessionStart" => Some(Self::SessionStart),
            "UserPromptSubmit" => Some(Self::UserPromptSubmit),
            "PreToolUse" => Some(Self::PreToolUse),
            "PermissionRequest" => Some(Self::PermissionRequest),
            "PostToolUse" => Some(Self::PostToolUse),
            "PreCompact" => Some(Self::PreCompact),
            "PostCompact" => Some(Self::PostCompact),
            "SubagentStart" => Some(Self::SubagentStart),
            "SubagentStop" => Some(Self::SubagentStop),
            "Stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HookStatus {
    pub event: String,
    pub matcher: Option<String>,
    pub source: String,
    pub command: Option<String>,
    pub hook_type: String,
    pub trusted: bool,
    pub enabled: bool,
    pub hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HookManager {
    workspace_root: PathBuf,
    data_dir: PathBuf,
    trust_path: PathBuf,
    disabled_path: PathBuf,
    inner: Arc<RwLock<HookState>>,
}

#[derive(Debug, Clone, Default)]
struct HookState {
    hooks: Vec<LoadedHook>,
    trusted: HashSet<String>,
    disabled: HashSet<String>,
}

#[derive(Debug, Clone)]
struct LoadedHook {
    event: HookEventName,
    matcher: Option<String>,
    handler: HookHandler,
    source: PathBuf,
    hash: String,
    warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookHandler {
    #[serde(rename = "type")]
    hook_type: String,
    command: Option<String>,
    command_windows: Option<String>,
    timeout: Option<u64>,
    status_message: Option<String>,
    #[serde(default)]
    r#async: bool,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone)]
pub struct HookContext {
    pub session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub turn_id: Option<String>,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolHookContext {
    pub tool_name: String,
    pub tool_use_id: String,
    pub tool_input: Value,
    pub tool_response: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    pub blocked: bool,
    pub reason: Option<String>,
    pub continue_flow: Option<bool>,
    pub permission_decision: Option<HookPermissionDecision>,
    pub updated_input: Option<Value>,
    pub additional_context: Vec<String>,
    pub failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPermissionDecision {
    Allow,
    Deny,
    Ask,
}

impl HookManager {
    pub fn new(workspace_root: PathBuf, data_dir: PathBuf) -> Arc<Self> {
        let manager = Arc::new(Self {
            workspace_root,
            trust_path: data_dir.join("hooks").join("trust.json"),
            disabled_path: data_dir.join("hooks").join("disabled.json"),
            data_dir,
            inner: Arc::new(RwLock::new(HookState::default())),
        });
        manager.reload_blocking();
        manager
    }

    fn reload_blocking(&self) {
        let hooks = discover_hooks(&self.workspace_root, &self.data_dir);
        let trusted = read_hash_set(&self.trust_path);
        let disabled = read_hash_set(&self.disabled_path);
        let state = HookState {
            hooks,
            trusted,
            disabled,
        };
        if let Ok(mut guard) = self.inner.try_write() {
            *guard = state;
        }
    }

    pub async fn reload(&self) {
        let hooks = discover_hooks(&self.workspace_root, &self.data_dir);
        let trusted = read_hash_set(&self.trust_path);
        let disabled = read_hash_set(&self.disabled_path);
        *self.inner.write().await = HookState {
            hooks,
            trusted,
            disabled,
        };
    }

    pub async fn statuses(&self) -> Vec<HookStatus> {
        self.reload().await;
        let state = self.inner.read().await;
        state
            .hooks
            .iter()
            .map(|hook| HookStatus {
                event: hook.event.as_str().to_string(),
                matcher: hook.matcher.clone(),
                source: hook.source.display().to_string(),
                command: hook.effective_command(),
                hook_type: hook.handler.hook_type.clone(),
                trusted: state.trusted.contains(&hook.hash),
                enabled: !state.disabled.contains(&hook.hash),
                hash: hook.hash.clone(),
                warning: hook.warning.clone(),
            })
            .collect()
    }

    pub async fn trust(&self, hash: &str) -> std::io::Result<bool> {
        let mut state = self.inner.write().await;
        let found = state.hooks.iter().any(|hook| hook.hash == hash);
        if found {
            state.trusted.insert(hash.to_string());
            write_hash_set(&self.trust_path, &state.trusted)?;
        }
        Ok(found)
    }

    pub async fn set_enabled(&self, hash: &str, enabled: bool) -> std::io::Result<bool> {
        let mut state = self.inner.write().await;
        let found = state.hooks.iter().any(|hook| hook.hash == hash);
        if found {
            if enabled {
                state.disabled.remove(hash);
            } else {
                state.disabled.insert(hash.to_string());
            }
            write_hash_set(&self.disabled_path, &state.disabled)?;
        }
        Ok(found)
    }

    pub async fn run(
        &self,
        event: HookEventName,
        matcher_target: Option<&str>,
        context: &HookContext,
        event_payload: Value,
    ) -> HookOutcome {
        let targets = matcher_target
            .map(|target| vec![target.to_string()])
            .unwrap_or_default();
        self.run_with_targets(event, &targets, context, event_payload)
            .await
    }

    pub async fn run_with_targets(
        &self,
        event: HookEventName,
        matcher_targets: &[String],
        context: &HookContext,
        event_payload: Value,
    ) -> HookOutcome {
        self.reload().await;
        let state = self.inner.read().await.clone();
        let hooks = state
            .hooks
            .into_iter()
            .filter(|hook| hook.event == event)
            .filter(|hook| hook.matches(matcher_targets))
            .filter(|hook| !state.disabled.contains(&hook.hash))
            .collect::<Vec<_>>();
        if hooks.is_empty() {
            return HookOutcome::default();
        }

        let mut tasks = Vec::new();
        for hook in hooks {
            if !state.trusted.contains(&hook.hash) {
                debug!(
                    hook_hash = %hook.hash,
                    event = event.as_str(),
                    "skipping untrusted hook"
                );
                continue;
            }
            tasks.push(tokio::spawn(run_hook(
                hook,
                context.clone(),
                event_payload.clone(),
            )));
        }

        let mut merged = HookOutcome::default();
        for task in tasks {
            match task.await {
                Ok(outcome) => merge_outcome(&mut merged, outcome),
                Err(err) => warn!(error = %err, event = event.as_str(), "hook task failed"),
            }
        }
        merged
    }

    pub async fn run_tool(
        &self,
        event: HookEventName,
        context: &HookContext,
        tool: &ToolHookContext,
    ) -> HookOutcome {
        let canonical_name = canonical_tool_name(&tool.tool_name);
        let tool_input = canonical_tool_input(&canonical_name, &tool.tool_input);
        let mut payload = json!({
            "tool_name": canonical_name,
            "tool_use_id": tool.tool_use_id,
            "tool_input": tool_input,
        });
        if let Some(response) = &tool.tool_response {
            payload["tool_response"] = response.clone();
        }
        let targets = matcher_aliases(&canonical_name);
        let mut outcome = self
            .run_with_targets(event, &targets, context, payload)
            .await;
        validate_tool_outcome(event, &canonical_name, &mut outcome);
        outcome
    }
}

impl LoadedHook {
    fn matches(&self, targets: &[String]) -> bool {
        if matches!(
            self.event,
            HookEventName::UserPromptSubmit | HookEventName::Stop
        ) {
            return true;
        }
        let Some(matcher) = self.matcher.as_deref().map(str::trim) else {
            return true;
        };
        if matcher.is_empty() || matcher == "*" {
            return true;
        }
        if targets.is_empty() {
            return false;
        }
        targets.iter().any(|target| {
            Regex::new(matcher)
                .map(|regex| regex.is_match(target))
                .unwrap_or_else(|_| matcher == target)
        })
    }

    fn effective_command(&self) -> Option<String> {
        #[cfg(windows)]
        {
            self.handler
                .command_windows
                .clone()
                .or_else(|| self.handler.command.clone())
        }
        #[cfg(not(windows))]
        {
            self.handler.command.clone()
        }
    }
}

async fn run_hook(hook: LoadedHook, context: HookContext, event_payload: Value) -> HookOutcome {
    if hook.handler.hook_type != "command" {
        warn!(
            hook_type = %hook.handler.hook_type,
            hook_hash = %hook.hash,
            "skipping unsupported hook type"
        );
        return HookOutcome::default();
    }
    if hook.handler.r#async {
        debug!(hook_hash = %hook.hash, "skipping async hook in v1");
        return HookOutcome::default();
    }
    let Some(command) = hook.effective_command() else {
        return HookOutcome::default();
    };

    let mut stdin = json!({
        "session_id": context.session_id,
        "transcript_path": context.transcript_path.as_ref().map(|p| p.display().to_string()),
        "cwd": context.cwd.display().to_string(),
        "hook_event_name": hook.event.as_str(),
        "model": context.model,
        "turn_id": context.turn_id,
        "permission_mode": context.permission_mode,
    });
    if let (Value::Object(base), Value::Object(extra)) = (&mut stdin, event_payload) {
        for (key, value) in extra {
            base.insert(key, value);
        }
    }
    let stdin_bytes = match serde_json::to_vec(&stdin) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!(error = %err, "failed to serialize hook stdin");
            return HookOutcome::default();
        }
    };

    let mut cmd = shell_command(&command);
    cmd.current_dir(&context.cwd);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!(error = %err, command = %command, "failed to spawn hook");
            return HookOutcome::default();
        }
    };
    if let Some(mut child_stdin) = child.stdin.take() {
        let _ = child_stdin.write_all(&stdin_bytes).await;
    }
    let timeout_secs = hook.handler.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let output = match timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            warn!(error = %err, command = %command, "hook command failed");
            return HookOutcome::default();
        }
        Err(_) => {
            warn!(
                command = %command,
                timeout_secs,
                "hook command timed out"
            );
            return HookOutcome::default();
        }
    };

    parse_hook_output(
        hook.event,
        output.status.code().unwrap_or_default(),
        String::from_utf8_lossy(&output.stdout).trim(),
        String::from_utf8_lossy(&output.stderr).trim(),
    )
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd.exe");
        cmd.arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

fn parse_hook_output(event: HookEventName, code: i32, stdout: &str, stderr: &str) -> HookOutcome {
    let mut outcome = HookOutcome::default();
    if code == 2 {
        outcome.blocked = true;
        outcome.permission_decision = match event {
            HookEventName::PermissionRequest => Some(HookPermissionDecision::Deny),
            _ => outcome.permission_decision,
        };
        outcome.reason = (!stderr.is_empty()).then(|| stderr.to_string());
    }
    if !stdout.is_empty() {
        if let Ok(value) = serde_json::from_str::<Value>(stdout) {
            apply_json_output(event, &mut outcome, &value);
        } else if matches!(
            event,
            HookEventName::SessionStart
                | HookEventName::SubagentStart
                | HookEventName::UserPromptSubmit
        ) {
            outcome.additional_context.push(stdout.to_string());
        } else if matches!(event, HookEventName::Stop | HookEventName::SubagentStop) {
            outcome.failed = true;
            outcome.reason = Some("plain text stdout is invalid for this hook event".to_string());
        }
    }
    outcome
}

fn apply_json_output(event: HookEventName, outcome: &mut HookOutcome, value: &Value) {
    if value
        .get("continue")
        .and_then(Value::as_bool)
        .is_some_and(|continue_flow| !continue_flow)
    {
        outcome.continue_flow = Some(false);
        outcome.blocked = true;
    }
    if value
        .get("decision")
        .and_then(Value::as_str)
        .is_some_and(|decision| decision == "block")
    {
        outcome.blocked = true;
        outcome.reason = value
            .get("reason")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| outcome.reason.clone());
    }
    let hook_specific = value
        .get("hookSpecificOutput")
        .and_then(Value::as_object)
        .map(|_| value.get("hookSpecificOutput").unwrap())
        .unwrap_or(value);

    if let Some(decision) = hook_specific
        .get("decision")
        .and_then(|value| value.get("behavior"))
        .and_then(Value::as_str)
        .and_then(parse_permission_decision)
    {
        outcome.permission_decision = Some(decision);
        if decision == HookPermissionDecision::Deny {
            outcome.blocked = true;
        }
    }
    if let Some(decision) = hook_specific
        .get("permissionDecision")
        .and_then(Value::as_str)
        .and_then(parse_permission_decision)
    {
        outcome.permission_decision = Some(decision);
        if decision == HookPermissionDecision::Deny {
            outcome.blocked = true;
        }
    }
    if let Some(reason) = hook_specific
        .get("permissionDecisionReason")
        .or_else(|| hook_specific.get("reason"))
        .and_then(Value::as_str)
    {
        outcome.reason = Some(reason.to_string());
    }
    if let Some(updated) = hook_specific.get("updatedInput") {
        outcome.updated_input = Some(updated.clone());
    }
    if let Some(context) = hook_specific
        .get("additionalContext")
        .and_then(Value::as_str)
    {
        outcome.additional_context.push(context.to_string());
    }
    if matches!(
        event,
        HookEventName::SessionStart
            | HookEventName::SubagentStart
            | HookEventName::UserPromptSubmit
    ) {
        if let Some(context) = value.get("additionalContext").and_then(Value::as_str) {
            outcome.additional_context.push(context.to_string());
        }
    }
}

fn parse_permission_decision(value: &str) -> Option<HookPermissionDecision> {
    match value {
        "allow" => Some(HookPermissionDecision::Allow),
        "deny" => Some(HookPermissionDecision::Deny),
        "ask" => Some(HookPermissionDecision::Ask),
        _ => None,
    }
}

fn merge_outcome(target: &mut HookOutcome, next: HookOutcome) {
    if next.blocked {
        target.blocked = true;
    }
    if next.failed {
        target.failed = true;
    }
    if next.continue_flow == Some(false) {
        target.continue_flow = Some(false);
    }
    if next.reason.is_some() {
        target.reason = next.reason;
    }
    match (target.permission_decision, next.permission_decision) {
        (_, Some(HookPermissionDecision::Deny)) => {
            target.permission_decision = Some(HookPermissionDecision::Deny)
        }
        (None, Some(decision)) | (Some(HookPermissionDecision::Ask), Some(decision)) => {
            target.permission_decision = Some(decision)
        }
        _ => {}
    }
    if next.updated_input.is_some() {
        target.updated_input = next.updated_input;
    }
    target.additional_context.extend(next.additional_context);
}

fn discover_hooks(workspace_root: &Path, data_dir: &Path) -> Vec<LoadedHook> {
    let mut hooks = Vec::new();
    for path in config_paths(workspace_root, data_dir) {
        if !hooks_enabled_for_config(&path) {
            continue;
        }
        let Some(value) = read_config_value(&path) else {
            continue;
        };
        hooks.extend(parse_hooks_value(&path, &value));
    }
    hooks
}

fn hooks_enabled_for_config(path: &Path) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
        return true;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return true;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return true;
    };
    let Some(features) = value.get("features").and_then(toml::Value::as_table) else {
        return true;
    };
    let hooks = features
        .get("hooks")
        .or_else(|| features.get("codex_hooks"))
        .and_then(toml::Value::as_bool);
    hooks.unwrap_or(true)
}

fn config_paths(workspace_root: &Path, data_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        paths.push(home.join(".codex").join("hooks.json"));
        paths.push(home.join(".codex").join("config.toml"));
    }
    paths.push(workspace_root.join(".codex").join("hooks.json"));
    paths.push(workspace_root.join(".codex").join("config.toml"));
    paths.push(data_dir.join("hooks.json"));
    paths
}

fn read_config_value(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => {
            let value: toml::Value = toml::from_str(&text).ok()?;
            serde_json::to_value(value.get("hooks")?).ok()
        }
        _ => serde_json::from_str(&text).ok(),
    }
}

fn parse_hooks_value(source: &Path, value: &Value) -> Vec<LoadedHook> {
    if value.get("hooks").is_some() {
        return parse_hooks_value(source, value.get("hooks").unwrap());
    }
    let mut hooks = Vec::new();
    let Some(map) = value.as_object() else {
        return hooks;
    };
    for (event_key, entries) in map {
        let Some(event) = HookEventName::from_key(event_key) else {
            continue;
        };
        let Some(entries) = entries.as_array() else {
            continue;
        };
        for entry in entries {
            let matcher = entry
                .get("matcher")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let handlers = entry
                .get("hooks")
                .and_then(Value::as_array)
                .cloned()
                .or_else(|| entry.as_array().cloned())
                .unwrap_or_default();
            for handler_value in handlers {
                let mut warning = None;
                let handler = match serde_json::from_value::<HookHandler>(handler_value.clone()) {
                    Ok(handler) => handler,
                    Err(err) => {
                        warning = Some(format!("invalid hook handler: {err}"));
                        HookHandler {
                            hook_type: "invalid".to_string(),
                            command: None,
                            command_windows: None,
                            timeout: None,
                            status_message: None,
                            r#async: false,
                            extra: BTreeMap::new(),
                        }
                    }
                };
                let hash = hook_hash(event, matcher.as_deref(), &handler_value);
                hooks.push(LoadedHook {
                    event,
                    matcher: matcher.clone(),
                    handler,
                    source: source.to_path_buf(),
                    hash,
                    warning,
                });
            }
        }
    }
    hooks
}

fn hook_hash(event: HookEventName, matcher: Option<&str>, handler: &Value) -> String {
    let value = json!({
        "event": event.as_str(),
        "matcher": matcher,
        "handler": handler,
    });
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn read_hash_set(path: &Path) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashSet::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn write_hash_set(path: &Path, values: &HashSet<String>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut values = values.iter().cloned().collect::<Vec<_>>();
    values.sort();
    std::fs::write(
        path,
        serde_json::to_string_pretty(&values).unwrap_or_default(),
    )
}

pub fn canonical_tool_name(name: &str) -> String {
    match name {
        "bash" => "Bash".to_string(),
        "apply_patch" | "fs_apply_patch" => "apply_patch".to_string(),
        other => other.to_string(),
    }
}

pub fn remi_tool_input_from_hook_update(
    remi_tool_name: &str,
    original_input: &Value,
    updated_input: Value,
) -> Value {
    match canonical_tool_name(remi_tool_name).as_str() {
        "apply_patch" => {
            let Some(command) = updated_input.get("command").and_then(Value::as_str) else {
                return updated_input;
            };
            let mut next = original_input.clone();
            if !next.is_object() {
                next = json!({});
            }
            if let Value::Object(map) = &mut next {
                map.insert("patch".to_string(), Value::String(command.to_string()));
            }
            next
        }
        _ => updated_input,
    }
}

fn matcher_aliases(canonical_name: &str) -> Vec<String> {
    match canonical_name {
        "apply_patch" => vec![
            "apply_patch".to_string(),
            "Edit".to_string(),
            "Write".to_string(),
        ],
        other => vec![other.to_string()],
    }
}

fn canonical_tool_input(canonical_name: &str, input: &Value) -> Value {
    match canonical_name {
        "Bash" | "apply_patch" => {
            if input.get("command").and_then(Value::as_str).is_some() {
                return input.clone();
            }
            if let Some(command) = input
                .get("cmd")
                .or_else(|| input.get("patch"))
                .or_else(|| input.get("input"))
                .and_then(Value::as_str)
            {
                return json!({ "command": command });
            }
            input.clone()
        }
        _ => input.clone(),
    }
}

fn validate_tool_outcome(event: HookEventName, canonical_name: &str, outcome: &mut HookOutcome) {
    let Some(updated_input) = outcome.updated_input.as_ref() else {
        return;
    };
    if event != HookEventName::PreToolUse
        || outcome.permission_decision != Some(HookPermissionDecision::Allow)
    {
        mark_invalid_updated_input(outcome);
        return;
    }
    if matches!(canonical_name, "Bash" | "apply_patch")
        && !updated_input
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| !command.is_empty())
    {
        mark_invalid_updated_input(outcome);
    }
}

fn mark_invalid_updated_input(outcome: &mut HookOutcome) {
    outcome.failed = true;
    outcome.updated_input = None;
    if outcome.permission_decision == Some(HookPermissionDecision::Allow) {
        outcome.permission_decision = None;
    }
    outcome.reason = Some("invalid updatedInput for hook event".to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_hooks_json_shape() {
        let value = json!({
            "PreToolUse": [
                {
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "echo ok", "timeout": 5}
                    ]
                }
            ]
        });
        let hooks = parse_hooks_value(Path::new("hooks.json"), &value);
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEventName::PreToolUse);
        assert_eq!(hooks[0].matcher.as_deref(), Some("Bash"));
        assert_eq!(hooks[0].handler.command.as_deref(), Some("echo ok"));
    }

    #[test]
    fn parses_permission_decision_from_hook_specific_output() {
        let value = json!({
            "hookSpecificOutput": {
                "permissionDecision": "allow",
                "updatedInput": {"command": "pwd"}
            }
        });
        let mut outcome = HookOutcome::default();
        apply_json_output(HookEventName::PreToolUse, &mut outcome, &value);
        assert_eq!(
            outcome.permission_decision,
            Some(HookPermissionDecision::Allow)
        );
        assert_eq!(outcome.updated_input, Some(json!({"command": "pwd"})));
    }

    #[test]
    fn parses_codex_permission_request_decision_behavior() {
        let value = json!({
            "hookSpecificOutput": {
                "decision": {
                    "behavior": "deny",
                    "message": "blocked"
                }
            }
        });
        let mut outcome = HookOutcome::default();
        apply_json_output(HookEventName::PermissionRequest, &mut outcome, &value);
        assert_eq!(
            outcome.permission_decision,
            Some(HookPermissionDecision::Deny)
        );
        assert!(outcome.blocked);
    }

    #[test]
    fn apply_patch_matches_codex_edit_write_aliases() {
        let hook = LoadedHook {
            event: HookEventName::PreToolUse,
            matcher: Some("Edit|Write".to_string()),
            handler: HookHandler {
                hook_type: "command".to_string(),
                command: Some("true".to_string()),
                command_windows: None,
                timeout: None,
                status_message: None,
                r#async: false,
                extra: BTreeMap::new(),
            },
            source: PathBuf::from("hooks.json"),
            hash: "hash".to_string(),
            warning: None,
        };
        assert!(hook.matches(&matcher_aliases("apply_patch")));
    }

    #[test]
    fn invalid_updated_input_is_ignored_for_bash() {
        let mut outcome = HookOutcome {
            permission_decision: Some(HookPermissionDecision::Allow),
            updated_input: Some(json!({"bad": true})),
            ..HookOutcome::default()
        };
        validate_tool_outcome(HookEventName::PreToolUse, "Bash", &mut outcome);
        assert!(outcome.failed);
        assert!(outcome.updated_input.is_none());
        assert!(outcome.permission_decision.is_none());
    }

    #[test]
    fn remi_apply_patch_update_uses_patch_argument() {
        let original = json!({"workdir": "crates"});
        let updated = remi_tool_input_from_hook_update(
            "apply_patch",
            &original,
            json!({"command": "--- a/a\n+++ b/a\n@@\n-old\n+new\n"}),
        );
        assert_eq!(
            updated,
            json!({"workdir": "crates", "patch": "--- a/a\n+++ b/a\n@@\n-old\n+new\n"})
        );
    }

    #[test]
    fn toml_feature_hooks_false_disables_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let codex_dir = dir.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let path = codex_dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
[features]
hooks = false

[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "echo blocked"
"#,
        )
        .unwrap();
        assert!(!hooks_enabled_for_config(&path));
        assert!(discover_hooks(dir.path(), dir.path()).is_empty());
    }
}
