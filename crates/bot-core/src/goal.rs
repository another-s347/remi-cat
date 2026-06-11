use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::supervisor_workflow::{self, WorkflowInstance, WorkflowMaxRounds, WorkflowStatus};

pub type GoalMaxRounds = WorkflowMaxRounds;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Completed,
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

pub fn from_instance(instance: &WorkflowInstance) -> Option<GoalState> {
    if instance.definition.id != "goal" {
        return None;
    }
    let goal = instance.context.get("goal")?.as_str()?.to_string();
    let last_evaluation = instance
        .last_report
        .as_ref()
        .map(|report| SupervisorDecision {
            status: if report.status == WorkflowStatus::Completed {
                SupervisorDecisionStatus::Completed
            } else {
                SupervisorDecisionStatus::Continue
            },
            message: report.agent_message.clone(),
            reason: report.reason.clone(),
        });
    Some(GoalState {
        goal,
        status: match instance.status {
            WorkflowStatus::Completed => GoalStatus::Completed,
            WorkflowStatus::Paused | WorkflowStatus::Stopped => GoalStatus::Paused,
            WorkflowStatus::Active | WorkflowStatus::Error => GoalStatus::Active,
        },
        max_rounds: instance.max_rounds.clone(),
        last_evaluation,
        updated_at: instance.updated_at,
    })
}

pub async fn migrate_legacy_goal(data_dir: &Path, thread_id: &str) -> Option<WorkflowInstance> {
    if let Some(instance) = supervisor_workflow::load_instance(data_dir, thread_id).await {
        return Some(instance);
    }
    let path = legacy_goal_path(data_dir, thread_id);
    let text = tokio::fs::read_to_string(&path).await.ok()?;
    let legacy: GoalState = serde_json::from_str(&text).ok()?;
    let definition = supervisor_workflow::embedded_goal_definition();
    let completed = legacy.status == GoalStatus::Completed;
    let last_report = legacy.last_evaluation.as_ref().map(|decision| {
        crate::supervisor_workflow::WorkflowReport {
            workflow_id: "goal".into(),
            workflow_name: definition.name.clone(),
            from_node: "review".into(),
            edge: Some(
                if decision.status == SupervisorDecisionStatus::Completed {
                    "complete"
                } else {
                    "continue"
                }
                .into(),
            ),
            to_node: if decision.status == SupervisorDecisionStatus::Completed {
                "stop"
            } else {
                "review"
            }
            .into(),
            status: if completed {
                WorkflowStatus::Completed
            } else {
                WorkflowStatus::Active
            },
            reason: decision.reason.clone(),
            agent_message: decision.message.clone(),
            next_node_message: decision.message.clone(),
            supervisor_trace: Vec::new(),
            round: 0,
            max_rounds: legacy.max_rounds.clone(),
            error: None,
        }
    });
    let instance = WorkflowInstance {
        current_node: if completed { "stop" } else { "review" }.into(),
        incoming_edge: legacy.last_evaluation.as_ref().map(|decision| {
            if decision.status == SupervisorDecisionStatus::Completed {
                "complete"
            } else {
                "continue"
            }
            .into()
        }),
        node_message: legacy.last_evaluation.and_then(|decision| decision.message),
        status: if completed {
            WorkflowStatus::Completed
        } else {
            WorkflowStatus::Active
        },
        max_rounds: legacy.max_rounds,
        last_report,
        context: serde_json::json!({"goal": legacy.goal}),
        definition,
        updated_at: legacy.updated_at,
    };
    if supervisor_workflow::save_instance(data_dir, thread_id, &instance)
        .await
        .is_ok()
    {
        let _ = tokio::fs::remove_file(path).await;
    }
    Some(instance)
}

pub async fn clear_legacy_goal(data_dir: &Path, thread_id: &str) -> Result<(), std::io::Error> {
    match tokio::fs::remove_file(legacy_goal_path(data_dir, thread_id)).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn legacy_goal_path(data_dir: &Path, thread_id: &str) -> PathBuf {
    data_dir
        .join("memory")
        .join(sanitize_id(thread_id))
        .join("goal.json")
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
