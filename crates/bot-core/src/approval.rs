use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolRiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRiskReview {
    pub risk: ToolRiskLevel,
    pub reason: String,
    pub concerns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolApprovalRequest {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub risk: ToolRiskLevel,
    pub args_summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<ToolRiskReview>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalDecision {
    Deny,
    AllowOnce,
    AllowSession,
    AllowSessionModelAuto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalResolution {
    Denied,
    Approved,
}

pub enum ApprovalWait {
    Immediate(ApprovalResolution),
    Pending(oneshot::Receiver<ToolApprovalDecision>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionGrant {
    AllowAll,
    ModelAuto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalSessionPolicy {
    Ask,
    AllowAll,
    ModelAuto,
}

impl ApprovalSessionPolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AllowAll => "allow",
            Self::ModelAuto => "auto",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Ask => "low auto-runs; medium/high ask for approval",
            Self::AllowAll => "all tool requests auto-run in this session",
            Self::ModelAuto => "low/medium auto-run after risk review; high asks for approval",
        }
    }
}

#[derive(Debug)]
struct PendingApproval {
    request: ToolApprovalRequest,
    tx: oneshot::Sender<ToolApprovalDecision>,
}

#[derive(Debug, Default)]
struct ApprovalState {
    pending: HashMap<String, PendingApproval>,
    grants: HashMap<String, SessionGrant>,
}

#[derive(Debug, Default)]
pub struct ToolApprovalManager {
    state: Mutex<ApprovalState>,
}

impl ToolApprovalManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub async fn decide(
        &self,
        approval_id: &str,
        decision: ToolApprovalDecision,
    ) -> Option<ToolApprovalRequest> {
        let pending = {
            let mut state = self.state.lock().await;
            state.pending.remove(approval_id)
        }?;
        let request = pending.request.clone();
        let _ = pending.tx.send(decision);
        tracing::info!(
            approval_id,
            decision = ?decision,
            session_id = %request.session_id,
            tool_name = %request.tool_name,
            "tool_approval.decide"
        );
        Some(request)
    }

    pub async fn grant_session(&self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        let mut state = self.state.lock().await;
        state
            .grants
            .insert(session_id.clone(), SessionGrant::AllowAll);
        tracing::info!(
            session_id = %session_id,
            grant = "allow_all",
            "tool_approval.session_grant"
        );
    }

    pub async fn set_session_policy(
        &self,
        session_id: impl Into<String>,
        policy: ApprovalSessionPolicy,
    ) {
        let session_id = session_id.into();
        let mut state = self.state.lock().await;
        match policy {
            ApprovalSessionPolicy::Ask => {
                state.grants.remove(&session_id);
            }
            ApprovalSessionPolicy::AllowAll => {
                state
                    .grants
                    .insert(session_id.clone(), SessionGrant::AllowAll);
            }
            ApprovalSessionPolicy::ModelAuto => {
                state
                    .grants
                    .insert(session_id.clone(), SessionGrant::ModelAuto);
            }
        }
        tracing::info!(
            session_id = %session_id,
            policy = policy.label(),
            "tool_approval.session_policy"
        );
    }

    pub async fn session_policy(&self, session_id: &str) -> ApprovalSessionPolicy {
        let state = self.state.lock().await;
        match state.grants.get(session_id) {
            Some(SessionGrant::AllowAll) => ApprovalSessionPolicy::AllowAll,
            Some(SessionGrant::ModelAuto) => ApprovalSessionPolicy::ModelAuto,
            None => ApprovalSessionPolicy::Ask,
        }
    }

    pub async fn request(
        &self,
        request: ToolApprovalRequest,
    ) -> (ApprovalResolution, Vec<ApprovalEvent>) {
        let (wait, mut events) = self.start_request(request.clone()).await;
        match wait {
            ApprovalWait::Immediate(resolution) => (resolution, events),
            ApprovalWait::Pending(rx) => {
                let decision = rx.await.unwrap_or(ToolApprovalDecision::Deny);
                let (resolution, event) = self.finish_request(&request, decision).await;
                events.push(event);
                (resolution, events)
            }
        }
    }

    pub async fn start_request(
        &self,
        mut request: ToolApprovalRequest,
    ) -> (ApprovalWait, Vec<ApprovalEvent>) {
        let mut events = Vec::new();
        if request.risk == ToolRiskLevel::Low {
            tracing::info!(
                approval_id = %request.id,
                session_id = %request.session_id,
                tool_name = %request.tool_name,
                risk = ?request.risk,
                reason = "low_risk",
                "tool_approval.immediate"
            );
            return (
                ApprovalWait::Immediate(ApprovalResolution::Approved),
                events,
            );
        }
        if self.session_allow_all(&request.session_id).await {
            tracing::info!(
                approval_id = %request.id,
                session_id = %request.session_id,
                tool_name = %request.tool_name,
                risk = ?request.risk,
                reason = "session_allow_all",
                "tool_approval.immediate"
            );
            return (
                ApprovalWait::Immediate(ApprovalResolution::Approved),
                events,
            );
        }

        let review = review_tool_risk(&request.tool_name, &request.args_summary, request.risk);
        request.review = Some(review.clone());
        events.push(ApprovalEvent::Requested(request.clone()));
        events.push(ApprovalEvent::Updated(request.clone()));

        if self.session_model_auto(&request.session_id).await && review.risk != ToolRiskLevel::High
        {
            tracing::info!(
                approval_id = %request.id,
                session_id = %request.session_id,
                tool_name = %request.tool_name,
                risk = ?request.risk,
                review_risk = ?review.risk,
                reason = "session_model_auto",
                decision = ?ToolApprovalDecision::AllowSessionModelAuto,
                "tool_approval.immediate"
            );
            events.push(ApprovalEvent::Resolved {
                request,
                decision: ToolApprovalDecision::AllowSessionModelAuto,
            });
            return (
                ApprovalWait::Immediate(ApprovalResolution::Approved),
                events,
            );
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            state.pending.insert(
                request.id.clone(),
                PendingApproval {
                    request: request.clone(),
                    tx,
                },
            );
        }
        tracing::info!(
            approval_id = %request.id,
            session_id = %request.session_id,
            tool_name = %request.tool_name,
            risk = ?request.risk,
            review_risk = ?review.risk,
            platform = request.platform.as_deref().unwrap_or(""),
            "tool_approval.pending"
        );
        (ApprovalWait::Pending(rx), events)
    }

    pub async fn finish_request(
        &self,
        request: &ToolApprovalRequest,
        decision: ToolApprovalDecision,
    ) -> (ApprovalResolution, ApprovalEvent) {
        {
            let mut state = self.state.lock().await;
            state.pending.remove(&request.id);
        }
        self.apply_decision(request, decision).await;
        let event = ApprovalEvent::Resolved {
            request: request.clone(),
            decision,
        };
        let resolution = match decision {
            ToolApprovalDecision::Deny => ApprovalResolution::Denied,
            ToolApprovalDecision::AllowOnce
            | ToolApprovalDecision::AllowSession
            | ToolApprovalDecision::AllowSessionModelAuto => ApprovalResolution::Approved,
        };
        tracing::info!(
            approval_id = %request.id,
            session_id = %request.session_id,
            tool_name = %request.tool_name,
            decision = ?decision,
            resolution = ?resolution,
            "tool_approval.resolved"
        );
        (resolution, event)
    }

    async fn session_allow_all(&self, session_id: &str) -> bool {
        let state = self.state.lock().await;
        state
            .grants
            .get(session_id)
            .is_some_and(|grant| *grant == SessionGrant::AllowAll)
    }

    async fn session_model_auto(&self, session_id: &str) -> bool {
        let state = self.state.lock().await;
        state
            .grants
            .get(session_id)
            .is_some_and(|grant| *grant == SessionGrant::ModelAuto)
    }

    async fn apply_decision(&self, request: &ToolApprovalRequest, decision: ToolApprovalDecision) {
        let mut state = self.state.lock().await;
        match decision {
            ToolApprovalDecision::Deny => {}
            ToolApprovalDecision::AllowOnce => {}
            ToolApprovalDecision::AllowSession => {
                state
                    .grants
                    .insert(request.session_id.clone(), SessionGrant::AllowAll);
            }
            ToolApprovalDecision::AllowSessionModelAuto => {
                state
                    .grants
                    .insert(request.session_id.clone(), SessionGrant::ModelAuto);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum ApprovalEvent {
    Requested(ToolApprovalRequest),
    Updated(ToolApprovalRequest),
    Resolved {
        request: ToolApprovalRequest,
        decision: ToolApprovalDecision,
    },
}

pub fn classify_tool_risk(tool_name: &str, args: &serde_json::Value) -> ToolRiskLevel {
    match tool_name {
        "fs_read"
        | "fs_ls"
        | "search"
        | "skill__get"
        | "skill__search"
        | "skill__read_resource"
        | "apply_patch"
        | "fetch"
        | "memory__get_detail"
        | "memory__recall"
        | "memory__upsert_named"
        | "todo__add"
        | "todo__complete"
        | "todo__list"
        | "todo__remove"
        | "todo__update"
        | "trigger__list"
        | "web_search"
        | "now"
        | "instant"
        | "lazy_wait"
        | "sleep" => ToolRiskLevel::Low,
        "workspace_bash" | "bash" => classify_bash_risk(args),
        "fs_write" | "fs_create" | "fs_mkdir" | "fs_remove" => {
            classify_fs_mutation_risk(tool_name, args)
        }
        "manage_yourself" => classify_manage_yourself_risk(args),
        name if name.starts_with("im__") || name.contains("send") || name.contains("upload") => {
            ToolRiskLevel::High
        }
        "agent__explorer" => ToolRiskLevel::Low,
        "acp__chat" => ToolRiskLevel::Medium,
        name if name.starts_with("agent__") || name.starts_with("trigger__") => {
            ToolRiskLevel::Medium
        }
        _ => ToolRiskLevel::Medium,
    }
}

fn classify_bash_risk(args: &serde_json::Value) -> ToolRiskLevel {
    let Some(command) = args.get("command").and_then(|value| value.as_str()) else {
        return ToolRiskLevel::High;
    };
    let Some(words) = shlex::split(command) else {
        return ToolRiskLevel::High;
    };
    let Some(program) = words.first().map(|word| {
        std::path::Path::new(word)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(word)
    }) else {
        return ToolRiskLevel::High;
    };
    match program {
        "ls" | "grep" | "rg" | "ripgrep" if !command_has_shell_control(command) => {
            ToolRiskLevel::Low
        }
        _ if is_destructive_command(command, &words) => ToolRiskLevel::High,
        _ => ToolRiskLevel::Medium,
    }
}

fn command_has_shell_control(command: &str) -> bool {
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '|' | ';' | '&' | '<' | '>' | '`' => return true,
            '$' if chars.peek().is_some_and(|next| *next == '(') => return true,
            _ => {}
        }
    }
    false
}

fn classify_fs_mutation_risk(tool_name: &str, args: &serde_json::Value) -> ToolRiskLevel {
    if tool_name == "fs_remove" {
        return ToolRiskLevel::High;
    }
    let path = args
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if touches_core_config_path(path) {
        return ToolRiskLevel::High;
    }
    if tool_name == "fs_remove"
        && (args["recursive"].as_bool().unwrap_or(false) || has_broad_wildcard(path))
    {
        return ToolRiskLevel::High;
    }
    ToolRiskLevel::Medium
}

fn classify_manage_yourself_risk(args: &serde_json::Value) -> ToolRiskLevel {
    let Some(command) = args.get("command").and_then(|value| value.as_str()) else {
        return ToolRiskLevel::High;
    };
    let Some(words) = shlex::split(command) else {
        return ToolRiskLevel::High;
    };
    match words.as_slice() {
        [top, sub] if top == "profile" && matches!(sub.as_str(), "list" | "show") => {
            ToolRiskLevel::Low
        }
        [top, sub, _profile] if top == "profile" && matches!(sub.as_str(), "show" | "status") => {
            ToolRiskLevel::Low
        }
        [top, sub, flag] if top == "profile" && sub == "status" && flag == "--all" => {
            ToolRiskLevel::Low
        }
        [top, category, sub, _profile]
            if top == "profile"
                && matches!(category.as_str(), "agent" | "workflow")
                && sub == "list" =>
        {
            ToolRiskLevel::Low
        }
        [top, category, sub, _profile, _id]
            if top == "profile"
                && matches!(category.as_str(), "agent" | "workflow")
                && sub == "show" =>
        {
            ToolRiskLevel::Low
        }
        _ => ToolRiskLevel::High,
    }
}

fn touches_core_config_path(path: &str) -> bool {
    let normalized = path.trim_start_matches("./").trim_start_matches('/');
    if normalized.is_empty() {
        return false;
    }
    let lower = normalized.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        ".env"
            | ".env.local"
            | ".env.production"
            | "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "dockerfile"
            | "docker-compose.yml"
            | "docker-compose.yaml"
            | ".git/config"
            | ".remi-cat/runtime.yaml"
            | ".remi-cat/runtime.yml"
    ) || lower.starts_with(".github/")
        || lower.starts_with(".cargo/")
        || lower.starts_with(".ssh/")
        || lower.starts_with("/etc/")
        || lower.starts_with("etc/")
}

fn has_broad_wildcard(path: &str) -> bool {
    let wildcard_count = path.chars().filter(|ch| matches!(ch, '*' | '?')).count();
    wildcard_count >= 2 || path.contains("**") || matches!(path.trim(), "*" | "*/*" | "./*")
}

fn is_destructive_command(command: &str, words: &[String]) -> bool {
    let lower = command.to_ascii_lowercase();
    if lower.contains("rm -rf")
        || lower.contains("rm -fr")
        || lower.contains("mkfs")
        || lower.contains("dd if=")
        || lower.contains(":(){")
        || lower.contains("chmod -r")
        || lower.contains("chown -r")
    {
        return true;
    }
    let Some(program) = words.first().map(|word| {
        std::path::Path::new(word)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(word)
    }) else {
        return true;
    };
    matches!(
        program,
        "rm" | "rmdir" | "mv" | "cp" | "truncate" | "dd" | "mkfs" | "chmod" | "chown" | "sudo"
    ) || words
        .iter()
        .skip(1)
        .any(|word| touches_core_config_path(word) || has_broad_wildcard(word))
}

pub fn summarize_tool_args(args: &serde_json::Value) -> String {
    let mut value = args.clone();
    redact_json_value(&mut value);
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "<invalid json>".into());
    const MAX: usize = 2_000;
    if text.len() <= MAX {
        text
    } else {
        format!("{}… [truncated {} bytes]", &text[..MAX], text.len() - MAX)
    }
}

fn redact_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                let key = key.to_ascii_lowercase();
                if key.contains("secret")
                    || key.contains("token")
                    || key.contains("password")
                    || key.contains("api_key")
                    || key.contains("authorization")
                {
                    *value = serde_json::Value::String("[redacted]".to_string());
                } else {
                    redact_json_value(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_json_value(value);
            }
        }
        _ => {}
    }
}

pub fn review_tool_risk(
    _tool_name: &str,
    _args_summary: &str,
    fallback: ToolRiskLevel,
) -> ToolRiskReview {
    let risk = fallback;
    ToolRiskReview {
        risk,
        reason: match risk {
            ToolRiskLevel::Low => "The request is read-only and scoped.".to_string(),
            ToolRiskLevel::Medium => {
                "The request may access external systems, delegate work, create future automation, or modify workspace files.".to_string()
            }
            ToolRiskLevel::High => {
                "The request can modify files, run commands, or send external messages.".to_string()
            }
        },
        concerns: match risk {
            ToolRiskLevel::Low => Vec::new(),
            ToolRiskLevel::Medium => vec!["Review external access or state changes.".to_string()],
            ToolRiskLevel::High => vec!["Requires explicit human confirmation.".to_string()],
        },
    }
}

pub fn parse_review_json(value: &str) -> ToolRiskReview {
    #[derive(Deserialize)]
    struct RawReview {
        risk: ToolRiskLevel,
        reason: Option<String>,
        concerns: Option<Vec<String>>,
    }

    serde_json::from_str::<RawReview>(value)
        .map(|raw| ToolRiskReview {
            risk: raw.risk,
            reason: raw.reason.unwrap_or_default(),
            concerns: raw.concerns.unwrap_or_default(),
        })
        .unwrap_or_else(|_| ToolRiskReview {
            risk: ToolRiskLevel::High,
            reason: "Risk review could not be parsed.".to_string(),
            concerns: vec!["Unparseable model review output.".to_string()],
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_policy_marks_readonly_low_and_dangerous_high() {
        assert_eq!(
            classify_tool_risk("fs_read", &serde_json::json!({})),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("todo__list", &serde_json::json!({})),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk(
                "todo__add",
                &serde_json::json!({ "title": "x", "items": [] })
            ),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk(
                "todo__update",
                &serde_json::json!({ "id": "1", "content": "x" })
            ),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("todo__complete", &serde_json::json!({ "id": "1" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("todo__remove", &serde_json::json!({ "id": "1" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("apply_patch", &serde_json::json!({})),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("skill__search", &serde_json::json!({ "query": "x" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("memory__recall", &serde_json::json!({ "query": "x" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("memory__upsert_named", &serde_json::json!({ "name": "x" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("web_search", &serde_json::json!({ "query": "x" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("sleep", &serde_json::json!({ "seconds": 1 })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk(
                "fetch",
                &serde_json::json!({ "url": "https://example.com" })
            ),
            ToolRiskLevel::Low
        );
        assert_eq!(
            review_tool_risk(
                "fetch",
                r#"{"url":"http://127.0.0.1:8080"}"#,
                ToolRiskLevel::Low
            )
            .risk,
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("agent__explorer", &serde_json::json!({ "task": "inspect" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("agent__coder", &serde_json::json!({ "task": "edit" })),
            ToolRiskLevel::Medium
        );
        assert_eq!(
            classify_tool_risk("fs_remove", &serde_json::json!({ "path": "x" })),
            ToolRiskLevel::High
        );
        assert_eq!(
            classify_tool_risk("fs_write", &serde_json::json!({ "path": "notes.txt" })),
            ToolRiskLevel::Medium
        );
        assert_eq!(
            classify_tool_risk("fs_write", &serde_json::json!({ "path": ".env" })),
            ToolRiskLevel::High
        );
        assert_eq!(
            classify_tool_risk("fs_remove", &serde_json::json!({ "path": "src/**/*" })),
            ToolRiskLevel::High
        );
        assert_eq!(
            classify_tool_risk(
                "fs_remove",
                &serde_json::json!({ "path": "target", "recursive": true })
            ),
            ToolRiskLevel::High
        );
        for command in [
            "profile list",
            "profile show default",
            "profile status default",
            "profile status --all",
            "profile agent list default",
            "profile agent show default coder",
            "profile workflow list default",
            "profile workflow show default goal",
        ] {
            assert_eq!(
                classify_tool_risk(
                    "manage_yourself",
                    &serde_json::json!({ "command": command })
                ),
                ToolRiskLevel::Low,
                "{command}"
            );
        }

        for command in [
            "profile create dev",
            "profile delete dev --force",
            "profile restart default",
            "profile agent upsert default /tmp/a.md",
            "profile workflow delete default foo",
        ] {
            assert_eq!(
                classify_tool_risk(
                    "manage_yourself",
                    &serde_json::json!({ "command": command })
                ),
                ToolRiskLevel::High,
                "{command}"
            );
        }
    }

    #[test]
    fn bash_policy_only_allows_simple_readonly_commands() {
        for command in [
            "ls",
            "ls -la",
            "grep needle file.txt",
            "rg needle",
            "ripgrep needle",
        ] {
            assert_eq!(
                classify_tool_risk("bash", &serde_json::json!({ "command": command })),
                ToolRiskLevel::Low,
                "{command}"
            );
        }

        for command in [
            "cat file.txt",
            "find . -type f",
            "rg needle | head",
            "grep $(whoami) file.txt",
        ] {
            assert_eq!(
                classify_tool_risk("bash", &serde_json::json!({ "command": command })),
                ToolRiskLevel::Medium,
                "{command}"
            );
        }

        for command in [
            "ls; rm -rf x",
            "rm file.txt",
            "rm -rf src/**/*",
            "sed -i s/a/b/ .env",
            "sudo apt-get remove libc6",
        ] {
            assert_eq!(
                classify_tool_risk("bash", &serde_json::json!({ "command": command })),
                ToolRiskLevel::High,
                "{command}"
            );
        }
    }

    #[test]
    fn review_parse_failure_is_high() {
        let review = parse_review_json("not json");
        assert_eq!(review.risk, ToolRiskLevel::High);
    }

    #[tokio::test]
    async fn session_model_auto_only_auto_approves_non_high() {
        let manager = ToolApprovalManager::new();
        let first = ToolApprovalRequest {
            id: "a1".into(),
            session_id: "s1".into(),
            run_id: "r1".into(),
            tool_call_id: "t1".into(),
            tool_name: "fetch".into(),
            risk: ToolRiskLevel::Medium,
            args_summary: "{}".into(),
            platform: None,
            review: None,
        };
        let (wait, _) = manager.start_request(first.clone()).await;
        let _ = manager
            .decide("a1", ToolApprovalDecision::AllowSessionModelAuto)
            .await;
        let decision = match wait {
            ApprovalWait::Pending(rx) => rx.await.expect("decision should be delivered"),
            ApprovalWait::Immediate(_) => panic!("medium request should wait before grant exists"),
        };
        let (resolution, _) = manager.finish_request(&first, decision).await;
        assert_eq!(resolution, ApprovalResolution::Approved);

        let (resolution, _) = manager
            .request(ToolApprovalRequest {
                id: "a2".into(),
                tool_call_id: "t2".into(),
                ..first.clone()
            })
            .await;
        assert_eq!(resolution, ApprovalResolution::Approved);

        let high = ToolApprovalRequest {
            id: "a3".into(),
            tool_call_id: "t3".into(),
            tool_name: "bash".into(),
            risk: ToolRiskLevel::High,
            ..first
        };
        let (wait, _) = manager.start_request(high.clone()).await;
        let _ = manager.decide("a3", ToolApprovalDecision::Deny).await;
        let decision = match wait {
            ApprovalWait::Pending(rx) => rx.await.expect("decision should be delivered"),
            ApprovalWait::Immediate(_) => panic!("high request should wait"),
        };
        let (resolution, _) = manager.finish_request(&high, decision).await;
        assert_eq!(resolution, ApprovalResolution::Denied);
    }

    #[tokio::test]
    async fn session_policy_can_be_configured_and_reset() {
        let manager = ToolApprovalManager::new();
        assert_eq!(
            manager.session_policy("s1").await,
            ApprovalSessionPolicy::Ask
        );

        manager
            .set_session_policy("s1", ApprovalSessionPolicy::ModelAuto)
            .await;
        assert_eq!(
            manager.session_policy("s1").await,
            ApprovalSessionPolicy::ModelAuto
        );
        let (resolution, events) = manager
            .request(ToolApprovalRequest {
                id: "p1".into(),
                session_id: "s1".into(),
                run_id: "r1".into(),
                tool_call_id: "t1".into(),
                tool_name: "fs_write".into(),
                risk: ToolRiskLevel::Medium,
                args_summary: "{}".into(),
                platform: None,
                review: None,
            })
            .await;
        assert_eq!(resolution, ApprovalResolution::Approved);
        assert!(events.iter().any(|event| matches!(
            event,
            ApprovalEvent::Resolved {
                decision: ToolApprovalDecision::AllowSessionModelAuto,
                ..
            }
        )));

        manager
            .set_session_policy("s1", ApprovalSessionPolicy::AllowAll)
            .await;
        assert_eq!(
            manager.session_policy("s1").await,
            ApprovalSessionPolicy::AllowAll
        );
        let (resolution, events) = manager
            .request(ToolApprovalRequest {
                id: "p2".into(),
                session_id: "s1".into(),
                run_id: "r1".into(),
                tool_call_id: "t2".into(),
                tool_name: "bash".into(),
                risk: ToolRiskLevel::High,
                args_summary: "{}".into(),
                platform: None,
                review: None,
            })
            .await;
        assert_eq!(resolution, ApprovalResolution::Approved);
        assert!(events.is_empty());

        manager
            .set_session_policy("s1", ApprovalSessionPolicy::Ask)
            .await;
        assert_eq!(
            manager.session_policy("s1").await,
            ApprovalSessionPolicy::Ask
        );
    }
}
