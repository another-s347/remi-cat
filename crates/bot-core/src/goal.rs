use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use remi_agentloop::prelude::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum GoalMaxRounds {
    Limited(u32),
    Unlimited,
}

impl Default for GoalMaxRounds {
    fn default() -> Self {
        Self::Limited(20)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoalState {
    pub goal: String,
    pub status: GoalStatus,
    #[serde(default)]
    pub max_rounds: GoalMaxRounds,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_evaluation: Option<SupervisorDecision>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorDecisionStatus {
    Completed,
    Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SupervisorDecision {
    pub status: SupervisorDecisionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorReport {
    pub goal: String,
    pub decision: SupervisorDecision,
    pub round: u32,
    pub max_rounds: GoalMaxRounds,
}

pub fn goal_path(data_dir: &Path, thread_id: &str) -> PathBuf {
    data_dir
        .join("memory")
        .join(sanitize_id(thread_id))
        .join("goal.json")
}

pub async fn load_goal(data_dir: &Path, thread_id: &str) -> Option<GoalState> {
    let path = goal_path(data_dir, thread_id);
    let text = tokio::fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&text).ok()
}

pub async fn save_goal(
    data_dir: &Path,
    thread_id: &str,
    goal: &GoalState,
) -> Result<(), std::io::Error> {
    let path = goal_path(data_dir, thread_id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let json = serde_json::to_vec_pretty(goal)?;
    tokio::fs::write(path, json).await
}

pub async fn clear_goal(data_dir: &Path, thread_id: &str) -> Result<(), std::io::Error> {
    match tokio::fs::remove_file(goal_path(data_dir, thread_id)).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn parse_supervisor_decision(raw: &str) -> Result<SupervisorDecision, String> {
    let trimmed = raw.trim();
    let json = extract_json_object(trimmed).ok_or_else(|| "missing JSON object".to_string())?;
    let mut decision: SupervisorDecision =
        serde_json::from_str(json).map_err(|err| format!("invalid supervisor JSON: {err}"))?;
    if decision.reason.trim().is_empty() {
        decision.reason = "no reason provided".to_string();
    } else {
        decision.reason = decision.reason.trim().to_string();
    }
    decision.message = decision
        .message
        .map(|message| message.trim().to_string())
        .filter(|message| !message.is_empty());
    Ok(decision)
}

pub fn supervisor_prompt(goal: &str, history: &[Message]) -> String {
    let history_json = serde_json::to_string_pretty(history).unwrap_or_else(|_| "[]".to_string());
    format!(
        "You are a supervisor agent. Evaluate whether the main agent has completed the goal.\n\
         You may use tools if they are needed to verify facts, files, commands, or external state.\n\
         Return only one JSON object with this exact shape:\n\
         {{\"status\":\"completed|continue\",\"message\":\"instruction for the main agent or null\",\"reason\":\"short reason\"}}\n\
         Use status=completed only when the goal is genuinely achieved. Use status=continue when more work is needed, and put the next concrete instruction for the main agent in message.\n\n\
         Goal:\n{goal}\n\n\
         Full conversation history JSON:\n{history_json}"
    )
}

pub fn format_goal_prefix(report: &SupervisorReport) -> String {
    let status = match report.decision.status {
        SupervisorDecisionStatus::Completed => "completed",
        SupervisorDecisionStatus::Continue => "continue",
    };
    format!(
        "**Goal**: {}\n\n**Supervisor**: {} - {}\n\n---\n\n",
        report.goal.trim(),
        status,
        report.decision.reason.trim()
    )
}

fn extract_json_object(raw: &str) -> Option<&str> {
    let without_fence = raw
        .strip_prefix("```json")
        .or_else(|| raw.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(raw);
    let start = without_fence.find('{')?;
    let end = without_fence.rfind('}')?;
    (start <= end).then(|| &without_fence[start..=end])
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_supervisor_json() {
        let decision = parse_supervisor_decision(
            r#"{"status":"continue","message":"Run tests","reason":"Not verified"}"#,
        )
        .unwrap();
        assert_eq!(decision.status, SupervisorDecisionStatus::Continue);
        assert_eq!(decision.message.as_deref(), Some("Run tests"));
    }

    #[test]
    fn parses_fenced_supervisor_json() {
        let decision = parse_supervisor_decision(
            "```json\n{\"status\":\"completed\",\"message\":null,\"reason\":\"done\"}\n```",
        )
        .unwrap();
        assert_eq!(decision.status, SupervisorDecisionStatus::Completed);
        assert!(decision.message.is_none());
    }
}
