use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{oneshot, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolRiskLevel {
    Low,
    Medium,
    High,
}

impl ToolRiskLevel {
    pub fn allows(self, risk: ToolRiskLevel) -> bool {
        self.rank() >= risk.rank()
    }

    fn rank(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRiskAssessment {
    Known(ToolRiskLevel),
    NeedsModelReview { reason: String },
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
    pub command_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_review_reason: Option<String>,
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
    #[serde(alias = "allow_session")]
    AllowSameCommandSession,
    #[serde(alias = "allow_session_model_auto")]
    AllowRiskLevelSession,
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
pub enum ApprovalSessionPolicy {
    Low,
    Medium,
}

impl ApprovalSessionPolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Low => "low risk tool requests auto-run; medium/high ask for approval",
            Self::Medium => "low and medium risk tool requests auto-run; high asks for approval",
        }
    }

    fn max_risk(self) -> ToolRiskLevel {
        match self {
            Self::Low => ToolRiskLevel::Low,
            Self::Medium => ToolRiskLevel::Medium,
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
    policies: HashMap<String, ApprovalSessionPolicy>,
    allowed_command_keys: HashMap<String, HashSet<String>>,
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
            .policies
            .insert(session_id.clone(), ApprovalSessionPolicy::Medium);
        tracing::info!(
            session_id = %session_id,
            grant = "medium",
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
        state.policies.insert(session_id.clone(), policy);
        tracing::info!(
            session_id = %session_id,
            policy = policy.label(),
            "tool_approval.session_policy"
        );
    }

    pub async fn session_policy(&self, session_id: &str) -> ApprovalSessionPolicy {
        let state = self.state.lock().await;
        state
            .policies
            .get(session_id)
            .copied()
            .unwrap_or(ApprovalSessionPolicy::Low)
    }

    pub async fn request(
        &self,
        request: ToolApprovalRequest,
    ) -> (ApprovalResolution, Vec<ApprovalEvent>) {
        let (wait, mut events) = self.start_request(request.clone()).await;
        let current_request = events
            .iter()
            .rev()
            .find_map(|event| match event {
                ApprovalEvent::Requested(request) | ApprovalEvent::Updated(request) => {
                    Some(request.clone())
                }
                ApprovalEvent::Resolved { request, .. } => Some(request.clone()),
            })
            .unwrap_or(request);
        match wait {
            ApprovalWait::Immediate(resolution) => (resolution, events),
            ApprovalWait::Pending(rx) => {
                let decision = rx.await.unwrap_or(ToolApprovalDecision::Deny);
                let (resolution, event) = self.finish_request(&current_request, decision).await;
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
        if self.session_command_auto_approves(&request).await {
            tracing::info!(
                approval_id = %request.id,
                session_id = %request.session_id,
                tool_name = %request.tool_name,
                risk = ?request.risk,
                reason = "command_key",
                "tool_approval.immediate"
            );
            return (
                ApprovalWait::Immediate(ApprovalResolution::Approved),
                events,
            );
        }

        let review = request.review.clone().unwrap_or_else(|| {
            review_tool_risk(&request.tool_name, &request.args_summary, request.risk)
        });
        request.risk = review.risk;
        request.review = Some(review.clone());
        if self.session_policy_auto_approves(&request).await {
            tracing::info!(
                approval_id = %request.id,
                session_id = %request.session_id,
                tool_name = %request.tool_name,
                risk = ?request.risk,
                reason = "session_policy",
                "tool_approval.immediate"
            );
            return (
                ApprovalWait::Immediate(ApprovalResolution::Approved),
                events,
            );
        }

        events.push(ApprovalEvent::Requested(request.clone()));
        events.push(ApprovalEvent::Updated(request.clone()));

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
            | ToolApprovalDecision::AllowSameCommandSession
            | ToolApprovalDecision::AllowRiskLevelSession => ApprovalResolution::Approved,
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

    async fn session_command_auto_approves(&self, request: &ToolApprovalRequest) -> bool {
        let state = self.state.lock().await;
        request.command_key.as_deref().is_some_and(|key| {
            state
                .allowed_command_keys
                .get(&request.session_id)
                .is_some_and(|keys| keys.contains(key))
        })
    }

    async fn session_policy_auto_approves(&self, request: &ToolApprovalRequest) -> bool {
        let state = self.state.lock().await;
        let policy = state
            .policies
            .get(&request.session_id)
            .copied()
            .unwrap_or(ApprovalSessionPolicy::Low);
        policy.max_risk().allows(request.risk)
    }

    async fn apply_decision(&self, request: &ToolApprovalRequest, decision: ToolApprovalDecision) {
        let mut state = self.state.lock().await;
        match decision {
            ToolApprovalDecision::Deny => {}
            ToolApprovalDecision::AllowOnce => {}
            ToolApprovalDecision::AllowSameCommandSession => {
                if let Some(command_key) = request.command_key.clone() {
                    state
                        .allowed_command_keys
                        .entry(request.session_id.clone())
                        .or_default()
                        .insert(command_key);
                }
            }
            ToolApprovalDecision::AllowRiskLevelSession => {
                if request.risk != ToolRiskLevel::High {
                    state.policies.insert(
                        request.session_id.clone(),
                        match request.risk {
                            ToolRiskLevel::Low => ApprovalSessionPolicy::Low,
                            ToolRiskLevel::Medium => ApprovalSessionPolicy::Medium,
                            ToolRiskLevel::High => ApprovalSessionPolicy::Medium,
                        },
                    );
                }
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
    match classify_tool_risk_assessment(tool_name, args) {
        ToolRiskAssessment::Known(risk) => risk,
        ToolRiskAssessment::NeedsModelReview { .. } => ToolRiskLevel::Medium,
    }
}

pub fn classify_tool_risk_assessment(
    tool_name: &str,
    args: &serde_json::Value,
) -> ToolRiskAssessment {
    let risk = match classify_known_tool_risk(tool_name, args) {
        Some(risk) => risk,
        None => {
            return ToolRiskAssessment::NeedsModelReview {
                reason: format!("No hard-coded approval rule matched `{tool_name}`."),
            };
        }
    };
    ToolRiskAssessment::Known(risk)
}

fn classify_known_tool_risk(tool_name: &str, args: &serde_json::Value) -> Option<ToolRiskLevel> {
    match tool_name {
        "fs_read" | "fs_ls" | "rg" | "search" | "skill__get" | "skill__search"
        | "memory__get_detail" | "memory__recall" | "todo__list" | "web_search" | "now"
        | "instant" | "lazy_wait" | "sleep" | "ask_user_question" | "tool_tasks" => {
            Some(ToolRiskLevel::Low)
        }
        "fetch" => Some(classify_fetch_risk(args)),
        "workspace_bash" | "bash" => Some(classify_bash_risk(args)),
        "ssh" => Some(classify_ssh_risk(args)),
        "fs_remove" => Some(classify_fs_remove_risk(args)),
        "fs_write" | "fs_create" | "fs_mkdir" | "apply_patch" => Some(ToolRiskLevel::Medium),
        "manage_yourself" => Some(classify_manage_yourself_risk(args)),
        name if name.starts_with("im__") || name.contains("send") || name.contains("upload") => {
            Some(ToolRiskLevel::Medium)
        }
        name if name.starts_with("agent__") => Some(ToolRiskLevel::Low),
        "codex" => Some(ToolRiskLevel::Medium),
        name if name.starts_with("acp__") => Some(ToolRiskLevel::Medium),
        name if name.starts_with("memory__")
            || name.starts_with("todo__")
            || name.starts_with("skill__") =>
        {
            Some(ToolRiskLevel::Medium)
        }
        _ => None,
    }
}

pub fn command_key(tool_name: &str, args: &serde_json::Value) -> String {
    let canonical_args = canonical_json(args);
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(canonical_args.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let body = entries
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into()),
                        canonical_json(value)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        serde_json::Value::Array(values) => {
            let body = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{body}]")
        }
        _ => serde_json::to_string(value).unwrap_or_else(|_| "null".into()),
    }
}

fn classify_bash_risk(args: &serde_json::Value) -> ToolRiskLevel {
    let Some(command) = shell_command_arg(args) else {
        return ToolRiskLevel::Medium;
    };
    if is_readonly_shell_command(command) {
        return ToolRiskLevel::Low;
    }
    if is_destructive_shell_command(command) {
        return ToolRiskLevel::High;
    }
    ToolRiskLevel::Medium
}

fn shell_command_arg(args: &serde_json::Value) -> Option<&str> {
    args.get("command")
        .or_else(|| args.get("cmd"))
        .and_then(|value| value.as_str())
}

fn classify_ssh_risk(args: &serde_json::Value) -> ToolRiskLevel {
    if args
        .get("pid")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        let action = args
            .get("action")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("poll");
        return match action {
            "poll" => ToolRiskLevel::Low,
            "cancel" => ToolRiskLevel::Medium,
            _ => ToolRiskLevel::Medium,
        };
    }
    classify_bash_risk(args)
}

fn classify_fetch_risk(args: &serde_json::Value) -> ToolRiskLevel {
    if args
        .get("task_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        ToolRiskLevel::Low
    } else {
        ToolRiskLevel::Medium
    }
}

fn is_readonly_shell_command(command: &str) -> bool {
    if command_has_non_readonly_shell_syntax(command) {
        return false;
    }
    let mut segments = split_readonly_shell_segments(command)
        .into_iter()
        .filter(|segment| !segment.trim().is_empty())
        .peekable();
    segments.peek().is_some() && segments.all(|segment| is_readonly_command_segment(segment.trim()))
}

fn command_has_non_readonly_shell_syntax(command: &str) -> bool {
    let mut quote = ShellQuote::None;
    let mut chars = command.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        match ch {
            '\\' if quote != ShellQuote::Single => {
                let _ = chars.next();
                continue;
            }
            '\'' if quote == ShellQuote::None => {
                quote = ShellQuote::Single;
                continue;
            }
            '\'' if quote == ShellQuote::Single => {
                quote = ShellQuote::None;
                continue;
            }
            '"' if quote == ShellQuote::None => {
                quote = ShellQuote::Double;
                continue;
            }
            '"' if quote == ShellQuote::Double => {
                quote = ShellQuote::None;
                continue;
            }
            _ => {}
        }

        match ch {
            '<' | '>' if quote == ShellQuote::None => return true,
            '`' if quote != ShellQuote::Single => return true,
            '&' if quote == ShellQuote::None => {
                if chars.peek().is_some_and(|(_, next)| *next == '&') {
                    let _ = chars.next();
                } else {
                    return true;
                }
            }
            '$' if quote != ShellQuote::Single
                && chars.peek().is_some_and(|(_, next)| *next == '(') =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellQuote {
    None,
    Single,
    Double,
}

fn split_readonly_shell_segments(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote = ShellQuote::None;
    let mut chars = command.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\\' if quote != ShellQuote::Single => {
                let _ = chars.next();
                continue;
            }
            '\'' if quote == ShellQuote::None => {
                quote = ShellQuote::Single;
                continue;
            }
            '\'' if quote == ShellQuote::Single => {
                quote = ShellQuote::None;
                continue;
            }
            '"' if quote == ShellQuote::None => {
                quote = ShellQuote::Double;
                continue;
            }
            '"' if quote == ShellQuote::Double => {
                quote = ShellQuote::None;
                continue;
            }
            _ => {}
        }
        if quote != ShellQuote::None {
            continue;
        }

        let split_len = match ch {
            '|' => {
                if chars.peek().is_some_and(|(_, next)| *next == '|') {
                    let _ = chars.next();
                    2
                } else {
                    1
                }
            }
            '&' if chars.peek().is_some_and(|(_, next)| *next == '&') => {
                let _ = chars.next();
                2
            }
            ';' => 1,
            _ => continue,
        };
        segments.push(&command[start..index]);
        start = index + split_len;
    }
    segments.push(&command[start..]);
    segments
}

fn is_destructive_shell_command(command: &str) -> bool {
    split_readonly_shell_segments(command)
        .into_iter()
        .filter(|segment| !segment.trim().is_empty())
        .any(|segment| {
            let Some(words) = shlex::split(segment.trim()) else {
                return true;
            };
            is_destructive_command(segment, &words)
        })
}

fn is_readonly_command_segment(segment: &str) -> bool {
    let Some(words) = shlex::split(segment) else {
        return false;
    };
    let Some(program) = words.first().map(|word| shell_program_name(word)) else {
        return false;
    };
    match program {
        "pwd" | "ls" | "cat" | "head" | "tail" | "wc" | "nl" | "sort" | "uniq" | "cut" | "tr"
        | "file" | "stat" | "du" | "grep" | "egrep" | "fgrep" | "rg" | "ripgrep" | "find"
        | "sed" | "git" | "cd" | "date" | "env" | "printenv" | "which" | "whereis" | "type"
        | "command" | "ps" | "df" | "free" | "uname" | "id" | "whoami" | "hostname"
        | "realpath" | "readlink" | "basename" | "dirname" | "jq" | "tree" => {
            is_readonly_program_invocation(program, &words)
        }
        _ => false,
    }
}

fn is_readonly_program_invocation(program: &str, words: &[String]) -> bool {
    match program {
        "find" => is_readonly_find_invocation(words),
        "sed" => !words
            .iter()
            .skip(1)
            .any(|word| word == "-i" || word.starts_with("-i")),
        "git" => words.get(1).is_some_and(|subcommand| {
            matches!(
                subcommand.as_str(),
                "status"
                    | "diff"
                    | "log"
                    | "show"
                    | "grep"
                    | "ls-files"
                    | "branch"
                    | "rev-parse"
                    | "describe"
                    | "blame"
                    | "shortlog"
            ) || words.get(1..=2).is_some_and(
                |parts| matches!(parts, [sub, action] if sub == "worktree" && action == "list"),
            ) || words.get(1..=2).is_some_and(
                |parts| matches!(parts, [sub, action] if sub == "stash" && action == "list"),
            ) || words.get(1..=2).is_some_and(
                |parts| matches!(parts, [sub, flag] if sub == "remote" && flag == "-v"),
            )
        }),
        "cd" => words.len() <= 2,
        "command" => words
            .get(1)
            .is_some_and(|arg| matches!(arg.as_str(), "-v" | "-V")),
        "type" => !words.iter().skip(1).any(|word| word == "-p"),
        _ => true,
    }
}

fn shell_program_name(word: &str) -> &str {
    std::path::Path::new(word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(word)
}

/// Find is treated as read-only when it only inspects files.
///
/// `-delete` is always destructive. `-exec/-execdir/-ok/-okdir` are:
/// - read-only when every executed program is on the read-only allowlist
/// - high-risk when any executed program is a known destructive tool
/// - medium otherwise (unknown / non-readonly program)
fn is_readonly_find_invocation(words: &[String]) -> bool {
    if words.iter().any(|word| word == "-delete") {
        return false;
    }
    find_exec_programs(words)
        .into_iter()
        .all(|(prog, args)| is_readonly_find_exec_program(prog, args))
}

fn find_action_is_destructive(words: &[String]) -> bool {
    if words.iter().any(|word| word == "-delete") {
        return true;
    }
    find_exec_programs(words)
        .into_iter()
        .any(|(prog, _args)| is_destructive_find_exec_program(prog))
}

fn is_destructive_find_exec_program(program: &str) -> bool {
    matches!(
        shell_program_name(program),
        "rm" | "rmdir" | "sudo" | "dd" | "mkfs" | "shred" | "chmod" | "chown" | "mv" | "cp"
            | "install" | "truncate" | "tee"
    ) || program.is_empty()
}

fn is_readonly_find_exec_program(program: &str, exec_args: &[String]) -> bool {
    if shell_program_name(program) == "sed" {
        return !exec_args.iter().any(|arg| arg == "-i");
    }
    matches!(
        shell_program_name(program),
        "pwd"
            | "ls"
            | "cat"
            | "head"
            | "tail"
            | "wc"
            | "nl"
            | "sort"
            | "uniq"
            | "cut"
            | "tr"
            | "file"
            | "stat"
            | "du"
            | "grep"
            | "egrep"
            | "fgrep"
            | "rg"
            | "ripgrep"
            | "date"
            | "env"
            | "printenv"
            | "which"
            | "whereis"
            | "type"
            | "command"
            | "ps"
            | "df"
            | "free"
            | "uname"
            | "id"
            | "whoami"
            | "hostname"
            | "realpath"
            | "readlink"
            | "basename"
            | "dirname"
            | "jq"
            | "tree"
    )
}

fn find_exec_programs(words: &[String]) -> Vec<(&str, &[String])> {
    let mut programs: Vec<(&str, &[String])> = Vec::new();
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        if !matches!(word, "-exec" | "-execdir" | "-ok" | "-okdir") {
            index += 1;
            continue;
        }
        let Some(program) = words.get(index + 1).map(String::as_str) else {
            // Missing program after an action predicate is not trusted as read-only.
            programs.push(("", &[]));
            break;
        };
        index += 2;
        let args_start = index;
        while index < words.len() && !matches!(words[index].as_str(), ";" | "+") {
            index += 1;
        }
        programs.push((program, &words[args_start..index]));
        if index < words.len() {
            index += 1;
        }
    }
    programs
}

fn classify_fs_remove_risk(args: &serde_json::Value) -> ToolRiskLevel {
    let path = args
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if is_high_risk_delete_path(path)
        || args["recursive"].as_bool().unwrap_or(false)
        || has_broad_wildcard(path)
    {
        return ToolRiskLevel::High;
    }
    ToolRiskLevel::Medium
}

fn classify_manage_yourself_risk(args: &serde_json::Value) -> ToolRiskLevel {
    let Some(command) = args.get("command").and_then(|value| value.as_str()) else {
        return ToolRiskLevel::Medium;
    };
    let Some(words) = shlex::split(command) else {
        return ToolRiskLevel::Medium;
    };
    match words.as_slice() {
        [top] if top == "tools" => ToolRiskLevel::Low,
        [top, flag] if top == "tools" && flag == "--json" => ToolRiskLevel::Low,
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
        _ => ToolRiskLevel::Medium,
    }
}

fn has_broad_wildcard(path: &str) -> bool {
    let wildcard_count = path.chars().filter(|ch| matches!(ch, '*' | '?')).count();
    wildcard_count >= 2
        || path.contains("**")
        || matches!(path.trim(), "*" | "*/*" | "./*" | "/*" | "/*/*")
}

fn is_high_risk_delete_path(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed == "/" {
        return true;
    }
    if !trimmed.starts_with('/') {
        return false;
    }
    let depth = trimmed
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .count();
    depth <= 2
}

fn is_destructive_command(command: &str, words: &[String]) -> bool {
    let lower = command.to_ascii_lowercase();
    if lower.contains("mkfs")
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
    if matches!(program, "dd" | "mkfs" | "sudo") {
        return true;
    }
    if matches!(program, "chmod" | "chown") && words.iter().any(|word| word == "-R" || word == "-r")
    {
        return true;
    }
    if matches!(program, "rm" | "rmdir") {
        let recursive = words
            .iter()
            .skip(1)
            .any(|word| word.starts_with('-') && word.contains('r'));
        return recursive
            || words
                .iter()
                .skip(1)
                .filter(|word| !word.starts_with('-'))
                .any(|word| is_high_risk_delete_path(word) || has_broad_wildcard(word));
    }
    if program == "find" {
        return find_action_is_destructive(words);
    }
    false
}

pub fn summarize_tool_args(args: &serde_json::Value) -> String {
    let mut value = args.clone();
    redact_json_value(&mut value);
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "<invalid json>".into());
    const MAX: usize = 2_000;
    if text.len() <= MAX {
        text
    } else {
        let split = safe_utf8_prefix_len(&text, MAX);
        format!(
            "{}… [truncated {} bytes]",
            &text[..split],
            text.len() - split
        )
    }
}

fn safe_utf8_prefix_len(text: &str, max_bytes: usize) -> usize {
    if max_bytes >= text.len() {
        return text.len();
    }
    let mut index = max_bytes;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
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
    tool_name: &str,
    args_summary: &str,
    fallback: ToolRiskLevel,
) -> ToolRiskReview {
    let risk = fallback;
    ToolRiskReview {
        risk,
        reason: review_reason(tool_name, args_summary, risk),
        concerns: review_concerns(tool_name, risk),
    }
}

fn review_reason(tool_name: &str, args_summary: &str, risk: ToolRiskLevel) -> String {
    match risk {
        ToolRiskLevel::Low if matches!(tool_name, "bash" | "workspace_bash" | "ssh") => {
            "The shell request is made only of recognized read-only inspection commands."
                .to_string()
        }
        ToolRiskLevel::Low => {
            "The tool request is classified as read-only and is safe to auto-run.".to_string()
        }
        ToolRiskLevel::Medium if matches!(tool_name, "bash" | "workspace_bash" | "ssh") => {
            "The shell request is not read-only, but no high-risk deletion or destructive system pattern was detected.".to_string()
        }
        ToolRiskLevel::Medium if matches!(tool_name, "fs_write" | "fs_create" | "fs_mkdir" | "fs_remove" | "apply_patch") => {
            "The request writes, creates, patches, or removes workspace content without matching the high-risk deletion rules.".to_string()
        }
        ToolRiskLevel::Medium
            if tool_name.starts_with("agent__")
                || tool_name == "codex"
                || tool_name.starts_with("acp__") =>
        {
            "The request delegates work to another agent and should be reviewed before running.".to_string()
        }
        ToolRiskLevel::Medium => {
            format!(
                "The `{tool_name}` request is not classified as read-only; review the summarized arguments before allowing it."
            )
        }
        ToolRiskLevel::High if matches!(tool_name, "bash" | "workspace_bash" | "ssh") => {
            "The shell request matches high-risk deletion or destructive system command rules.".to_string()
        }
        ToolRiskLevel::High if tool_name == "fs_remove" => {
            "The request matches high-risk deletion rules and requires human confirmation.".to_string()
        }
        ToolRiskLevel::High if args_summary.to_ascii_lowercase().contains("[redacted]") => {
            "The request touches sensitive-looking arguments and requires human confirmation."
                .to_string()
        }
        ToolRiskLevel::High => {
            format!("The `{tool_name}` request is high risk and requires human confirmation.")
        }
    }
}

fn review_concerns(tool_name: &str, risk: ToolRiskLevel) -> Vec<String> {
    match risk {
        ToolRiskLevel::Low => Vec::new(),
        ToolRiskLevel::Medium if matches!(tool_name, "bash" | "workspace_bash" | "ssh") => {
            vec!["Command is outside the read-only shell allowlist.".to_string()]
        }
        ToolRiskLevel::Medium => {
            vec!["Confirm the requested state change is intended.".to_string()]
        }
        ToolRiskLevel::High => vec!["Requires explicit human confirmation.".to_string()],
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

#[derive(Clone)]
pub struct ModelApprovalReviewer {
    client: reqwest::Client,
    profile: crate::model_profile::ModelProfileConfig,
    api_key: String,
}

impl ModelApprovalReviewer {
    pub fn new(profile: crate::model_profile::ModelProfileConfig, api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            profile,
            api_key,
        }
    }

    pub async fn review(&self, request: &ToolApprovalRequest) -> ToolRiskReview {
        let endpoint = format!(
            "{}/chat/completions",
            approval_model_base_url(&self.profile)
        );
        let prompt = approval_review_prompt(request);
        let body = serde_json::json!({
            "model": self.profile.model,
            "messages": [
                {
                    "role": "system",
                    "content": "You review tool calls for a coding agent. Return only compact JSON with keys risk, reason, concerns. risk must be low, medium, or high."
                },
                { "role": "user", "content": prompt }
            ],
            "temperature": 0,
            "max_tokens": 300
        });
        let result = self
            .client
            .post(endpoint)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .and_then(|response| response.error_for_status());
        let Ok(response) = result else {
            return ToolRiskReview {
                risk: ToolRiskLevel::High,
                reason: "Approval model request failed.".to_string(),
                concerns: vec!["Requires explicit human confirmation.".to_string()],
            };
        };
        let Ok(value) = response.json::<serde_json::Value>().await else {
            return ToolRiskReview {
                risk: ToolRiskLevel::High,
                reason: "Approval model response could not be parsed.".to_string(),
                concerns: vec!["Requires explicit human confirmation.".to_string()],
            };
        };
        let content = value
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())
            .unwrap_or_default();
        parse_review_json(content)
    }
}

fn approval_model_base_url(profile: &crate::model_profile::ModelProfileConfig) -> String {
    profile
        .base_url
        .as_deref()
        .unwrap_or("https://api.openai.com/v1")
        .trim_end_matches('/')
        .to_string()
}

fn approval_review_prompt(request: &ToolApprovalRequest) -> String {
    format!(
        "Classify this tool call risk.\n\nTool: {}\nPlatform: {}\nHard-coded rule result: {}\nArguments:\n{}\n\nReturn JSON like {{\"risk\":\"medium\",\"reason\":\"...\",\"concerns\":[\"...\"]}}.",
        request.tool_name,
        request.platform.as_deref().unwrap_or("unknown"),
        request
            .model_review_reason
            .as_deref()
            .unwrap_or("No hard-coded rule matched."),
        request.args_summary
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_policy_marks_readonly_low_writes_medium_and_only_dangerous_deletes_high() {
        assert_eq!(
            classify_tool_risk("fs_read", &serde_json::json!({})),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("todo__list", &serde_json::json!({})),
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
            classify_tool_risk("web_search", &serde_json::json!({ "query": "x" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("sleep", &serde_json::json!({ "seconds": 1 })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("fetch", &serde_json::json!({ "task_id": "fetch_1" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk(
                "fetch",
                &serde_json::json!({ "url": "https://example.com" })
            ),
            ToolRiskLevel::Medium
        );
        assert_eq!(
            classify_tool_risk(
                "ask_user_question",
                &serde_json::json!({ "question": "ok?" })
            ),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("agent__explorer", &serde_json::json!({ "task": "inspect" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk("agent__coder", &serde_json::json!({ "task": "edit" })),
            ToolRiskLevel::Low
        );

        for (tool, args) in [
            ("apply_patch", serde_json::json!({ "patch": "..." })),
            ("fs_write", serde_json::json!({ "path": ".env" })),
            ("fs_create", serde_json::json!({ "path": "notes.txt" })),
            ("fs_mkdir", serde_json::json!({ "path": "tmp/work" })),
            ("memory__upsert_named", serde_json::json!({ "name": "x" })),
            (
                "todo__add",
                serde_json::json!({ "title": "x", "items": [] }),
            ),
            (
                "todo__update",
                serde_json::json!({ "id": "1", "content": "x" }),
            ),
            ("todo__complete", serde_json::json!({ "id": "1" })),
            ("todo__remove", serde_json::json!({ "id": "1" })),
            ("codex", serde_json::json!({ "message": "work" })),
            ("im__send_text", serde_json::json!({ "text": "hello" })),
        ] {
            assert_eq!(
                classify_tool_risk(tool, &args),
                ToolRiskLevel::Medium,
                "{tool}"
            );
        }

        assert_eq!(
            classify_tool_risk("fs_remove", &serde_json::json!({ "path": "x" })),
            ToolRiskLevel::Medium
        );
        assert_eq!(
            classify_tool_risk(
                "fs_remove",
                &serde_json::json!({ "path": "src/file.rs", "recursive": false })
            ),
            ToolRiskLevel::Medium
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
        for args in [
            serde_json::json!({ "path": "/" }),
            serde_json::json!({ "path": "/etc" }),
            serde_json::json!({ "path": "/etc/nginx" }),
            serde_json::json!({ "path": "src/**/*" }),
            serde_json::json!({ "path": "target", "recursive": true }),
        ] {
            assert_eq!(
                classify_tool_risk("fs_remove", &args),
                ToolRiskLevel::High,
                "{args}"
            );
        }
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
        assert_eq!(
            classify_tool_risk(
                "bash",
                &serde_json::json!({ "cmd": "grep needle file.txt" })
            ),
            ToolRiskLevel::Low
        );

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
                ToolRiskLevel::Medium,
                "{command}"
            );
        }
    }

    #[test]
    fn bash_policy_only_allows_simple_readonly_commands() {
        for command in [
            "ls",
            "ls -la",
            "pwd",
            "cat file.txt",
            "head -n 20 src/main.rs",
            "tail -n 20 src/main.rs",
            "wc -l src/main.rs",
            "find . -type f",
            "find . -type f -exec ls -lh {} ;",
            "find . -type f -exec cat {} +",
            "find . -name '*.rs' -exec rg -n needle {} +",
            "sed -n 1,20p src/main.rs",
            "grep needle file.txt",
            "rg needle",
            "ripgrep needle",
            "rg needle | head",
            r#"grep -n "tracing::info\|tracing::warn\|log::info\|println!" src/app.rs | head -20"#,
            "cd /workspace",
            "cd /workspace && pwd",
            "ls && pwd",
            "git worktree list",
            "git remote -v",
            "date",
            "whoami",
            "ps -ef",
            "df -h",
            "jq . package.json",
            "grep needle file.txt | wc -l",
            "git status --short",
            "git diff -- src/main.rs",
        ] {
            assert_eq!(
                classify_tool_risk("bash", &serde_json::json!({ "command": command })),
                ToolRiskLevel::Low,
                "{command}"
            );
        }

        for command in [
            "ls; rm -rf x",
            "rm -rf src/**/*",
            "rm /tmp/x",
            "rm -rf /tmp/x",
            "find . -type f -delete",
            "find . -type f -exec rm {} +",
            "find . -type f -ok rm {} ;",
            "sudo apt-get remove libc6",
        ] {
            assert_eq!(
                classify_tool_risk("bash", &serde_json::json!({ "command": command })),
                ToolRiskLevel::High,
                "{command}"
            );
        }

        for command in [
            "rm file.txt",
            "sed -i s/a/b/ .env",
            "find . -type f -exec sed -i s/a/b/ {} +",
            "grep $(whoami) file.txt",
            "python3 script.py",
            "cargo test -p bot-core",
            "touch file.txt",
            "find . -type f 2>/dev/null",
            "find . -type f -exec python3 script.py {} +",
            "find . -type f -execdir custom_tool {} +",
            "echo hi > file.txt",
        ] {
            assert_eq!(
                classify_tool_risk("bash", &serde_json::json!({ "command": command })),
                ToolRiskLevel::Medium,
                "{command}"
            );
        }
    }

    #[test]
    fn ssh_policy_reuses_bash_command_classification_and_handles_tasks() {
        assert_eq!(
            classify_tool_risk(
                "ssh",
                &serde_json::json!({ "host": "prod", "command": "ls -la" })
            ),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk(
                "ssh",
                &serde_json::json!({ "host": "prod", "command": "rm -rf /tmp/x" })
            ),
            ToolRiskLevel::High
        );
        assert_eq!(
            classify_tool_risk(
                "ssh",
                &serde_json::json!({ "host": "prod", "command": "rm file.txt" })
            ),
            ToolRiskLevel::Medium
        );
        assert_eq!(
            classify_tool_risk("ssh", &serde_json::json!({ "pid": "ssh_1" })),
            ToolRiskLevel::Low
        );
        assert_eq!(
            classify_tool_risk(
                "ssh",
                &serde_json::json!({ "pid": "ssh_1", "action": "cancel" })
            ),
            ToolRiskLevel::Medium
        );
    }

    #[tokio::test]
    async fn approval_manager_auto_runs_readonly_bash_but_blocks_unknown_bash() {
        let manager = ToolApprovalManager::new();
        let readonly = ToolApprovalRequest {
            id: "readonly".to_string(),
            session_id: "s".to_string(),
            run_id: "r".to_string(),
            tool_call_id: "tc1".to_string(),
            tool_name: "bash".to_string(),
            risk: classify_tool_risk(
                "bash",
                &serde_json::json!({ "command": "rg classify_tool_risk crates/bot-core/src/approval.rs | head" }),
            ),
            args_summary:
                r#"{"command":"rg classify_tool_risk crates/bot-core/src/approval.rs | head"}"#
                    .to_string(),
            command_key: Some(command_key(
                "bash",
                &serde_json::json!({ "command": "rg classify_tool_risk crates/bot-core/src/approval.rs | head" }),
            )),
            model_review_reason: None,
            platform: Some("test".to_string()),
            review: None,
        };

        let (wait, events) = manager.start_request(readonly).await;
        assert!(matches!(
            wait,
            ApprovalWait::Immediate(ApprovalResolution::Approved)
        ));
        assert!(events.is_empty());

        let unknown = ToolApprovalRequest {
            id: "unknown".to_string(),
            session_id: "s".to_string(),
            run_id: "r".to_string(),
            tool_call_id: "tc2".to_string(),
            tool_name: "bash".to_string(),
            risk: classify_tool_risk(
                "bash",
                &serde_json::json!({ "command": "cargo test -p bot-core" }),
            ),
            args_summary: r#"{"command":"cargo test -p bot-core"}"#.to_string(),
            command_key: Some(command_key(
                "bash",
                &serde_json::json!({ "command": "cargo test -p bot-core" }),
            )),
            model_review_reason: None,
            platform: Some("test".to_string()),
            review: None,
        };

        let (wait, events) = manager.start_request(unknown).await;
        assert!(matches!(wait, ApprovalWait::Pending(_)));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                ApprovalEvent::Updated(request)
                    if request.review.as_ref().is_some_and(|review| review.risk == ToolRiskLevel::Medium)
            )
        }));
    }

    #[test]
    fn review_parse_failure_is_high() {
        let review = parse_review_json("not json");
        assert_eq!(review.risk, ToolRiskLevel::High);
    }

    #[test]
    fn unknown_tool_requests_model_review() {
        assert!(matches!(
            classify_tool_risk_assessment("custom__danger_or_safe", &serde_json::json!({})),
            ToolRiskAssessment::NeedsModelReview { .. }
        ));
        assert_eq!(
            classify_tool_risk("custom__danger_or_safe", &serde_json::json!({})),
            ToolRiskLevel::Medium
        );
    }

    #[test]
    fn summarize_tool_args_truncates_multibyte_text_without_panicking() {
        let summary = summarize_tool_args(&serde_json::json!({
            "comment": "审批通过".repeat(600),
        }));

        assert!(summary.contains("审批"));
        assert!(summary.contains("[truncated "));
        assert!(summary.is_char_boundary(summary.len()));
    }

    #[tokio::test]
    async fn same_command_session_grant_only_approves_matching_command() {
        let manager = ToolApprovalManager::new();
        let first = ToolApprovalRequest {
            id: "a1".into(),
            session_id: "s1".into(),
            run_id: "r1".into(),
            tool_call_id: "t1".into(),
            tool_name: "fetch".into(),
            risk: ToolRiskLevel::Medium,
            args_summary: "{}".into(),
            command_key: Some("fetch:{}:one".into()),
            model_review_reason: None,
            platform: None,
            review: None,
        };
        let (wait, _) = manager.start_request(first.clone()).await;
        let _ = manager
            .decide("a1", ToolApprovalDecision::AllowSameCommandSession)
            .await;
        let decision = match wait {
            ApprovalWait::Pending(rx) => rx.await.expect("decision should be delivered"),
            ApprovalWait::Immediate(_) => panic!("medium request should wait before grant exists"),
        };
        let (resolution, _) = manager.finish_request(&first, decision).await;
        assert_eq!(resolution, ApprovalResolution::Approved);

        let (wait, events) = manager
            .start_request(ToolApprovalRequest {
                id: "a2".into(),
                tool_call_id: "t2".into(),
                ..first.clone()
            })
            .await;
        assert!(matches!(
            wait,
            ApprovalWait::Immediate(ApprovalResolution::Approved)
        ));
        assert!(events.is_empty());

        let different = ToolApprovalRequest {
            id: "a3".into(),
            tool_call_id: "t3".into(),
            command_key: Some("fetch:{}:different".into()),
            ..first.clone()
        };
        let (wait, _) = manager.start_request(different.clone()).await;
        let _ = manager.decide("a3", ToolApprovalDecision::Deny).await;
        let decision = match wait {
            ApprovalWait::Pending(rx) => rx.await.expect("decision should be delivered"),
            ApprovalWait::Immediate(_) => panic!("different command should wait"),
        };
        let (resolution, _) = manager.finish_request(&different, decision).await;
        assert_eq!(resolution, ApprovalResolution::Denied);
    }

    #[tokio::test]
    async fn risk_level_session_grant_uses_reviewed_risk_for_later_auto_approval() {
        let manager = ToolApprovalManager::new();
        let first = ToolApprovalRequest {
            id: "reviewed-a1".into(),
            session_id: "reviewed-session".into(),
            run_id: "r1".into(),
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            risk: ToolRiskLevel::High,
            args_summary: r#"{"cmd":"cargo test -p bot-core"}"#.into(),
            command_key: Some("bash:cargo-test:first".into()),
            model_review_reason: None,
            platform: None,
            review: Some(ToolRiskReview {
                risk: ToolRiskLevel::Medium,
                reason: "Reviewed as medium.".into(),
                concerns: Vec::new(),
            }),
        };

        let (wait, events) = manager.start_request(first.clone()).await;
        let reviewed_first = events
            .iter()
            .rev()
            .find_map(|event| match event {
                ApprovalEvent::Requested(request) | ApprovalEvent::Updated(request) => {
                    Some(request.clone())
                }
                ApprovalEvent::Resolved { .. } => None,
            })
            .expect("pending approval should publish reviewed request");
        assert_eq!(reviewed_first.risk, ToolRiskLevel::Medium);

        let _ = manager
            .decide("reviewed-a1", ToolApprovalDecision::AllowRiskLevelSession)
            .await;
        let decision = match wait {
            ApprovalWait::Pending(rx) => rx.await.expect("decision should be delivered"),
            ApprovalWait::Immediate(_) => panic!("first reviewed medium request should wait"),
        };
        let (resolution, _) = manager.finish_request(&reviewed_first, decision).await;
        assert_eq!(resolution, ApprovalResolution::Approved);

        let (wait, events) = manager
            .start_request(ToolApprovalRequest {
                id: "reviewed-a2".into(),
                tool_call_id: "t2".into(),
                command_key: Some("bash:cargo-test:second".into()),
                ..first
            })
            .await;
        assert!(matches!(
            wait,
            ApprovalWait::Immediate(ApprovalResolution::Approved)
        ));
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn session_policy_can_be_configured_and_reset() {
        let manager = ToolApprovalManager::new();
        assert_eq!(
            manager.session_policy("s1").await,
            ApprovalSessionPolicy::Low
        );

        manager
            .set_session_policy("s1", ApprovalSessionPolicy::Low)
            .await;
        assert_eq!(
            manager.session_policy("s1").await,
            ApprovalSessionPolicy::Low
        );
        let (wait, events) = manager
            .start_request(ToolApprovalRequest {
                id: "p1".into(),
                session_id: "s1".into(),
                run_id: "r1".into(),
                tool_call_id: "t1".into(),
                tool_name: "fs_write".into(),
                risk: ToolRiskLevel::Medium,
                args_summary: "{}".into(),
                command_key: Some("fs_write:{}:one".into()),
                model_review_reason: None,
                platform: None,
                review: None,
            })
            .await;
        assert!(matches!(wait, ApprovalWait::Pending(_)));
        assert!(events.iter().any(|event| matches!(
            event,
            ApprovalEvent::Updated(request)
                if request.review.as_ref().is_some_and(|review| review.risk == ToolRiskLevel::Medium)
        )));

        manager
            .set_session_policy("s1", ApprovalSessionPolicy::Medium)
            .await;
        assert_eq!(
            manager.session_policy("s1").await,
            ApprovalSessionPolicy::Medium
        );
        let (resolution, events) = manager
            .request(ToolApprovalRequest {
                id: "p2".into(),
                session_id: "s1".into(),
                run_id: "r1".into(),
                tool_call_id: "t2".into(),
                tool_name: "bash".into(),
                risk: ToolRiskLevel::Medium,
                args_summary: "{}".into(),
                command_key: Some("bash:{}:one".into()),
                model_review_reason: None,
                platform: None,
                review: None,
            })
            .await;
        assert_eq!(resolution, ApprovalResolution::Approved);
        assert!(events.is_empty());

        let (wait, _) = manager
            .start_request(ToolApprovalRequest {
                id: "p3".into(),
                session_id: "s1".into(),
                run_id: "r1".into(),
                tool_call_id: "t3".into(),
                tool_name: "bash".into(),
                risk: ToolRiskLevel::High,
                args_summary: "{}".into(),
                command_key: Some("bash:{}:high".into()),
                model_review_reason: None,
                platform: None,
                review: None,
            })
            .await;
        assert!(matches!(wait, ApprovalWait::Pending(_)));

        manager
            .set_session_policy("s1", ApprovalSessionPolicy::Low)
            .await;
        assert_eq!(
            manager.session_policy("s1").await,
            ApprovalSessionPolicy::Low
        );
    }
}
