use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use remi_agentloop::prelude::Message;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum WorkflowMaxRounds {
    Limited(u32),
    Unlimited,
}

impl Default for WorkflowMaxRounds {
    fn default() -> Self {
        Self::Limited(20)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowDefinition {
    pub version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub prompt: String,
    pub start_prompt: String,
    pub initial_node: String,
    pub terminal_node: String,
    pub nodes: Vec<WorkflowNode>,
    pub edges: Vec<WorkflowEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowNode {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowEdge {
    pub id: String,
    pub from: String,
    pub to: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Active,
    Paused,
    Completed,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDecision {
    pub edge: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_node_message: Option<String>,
    pub reason: String,
    #[serde(skip)]
    pub trace: Vec<SupervisorTraceEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SupervisorTraceEvent {
    Thinking {
        content: String,
    },
    ToolCall {
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        name: String,
        result: String,
    },
    OutputDelta {
        content: String,
    },
    Output {
        content: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowReport {
    pub workflow_id: String,
    pub workflow_name: String,
    pub from_node: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
    pub to_node: String,
    pub status: WorkflowStatus,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_node_message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supervisor_trace: Vec<SupervisorTraceEvent>,
    pub round: u32,
    pub max_rounds: WorkflowMaxRounds,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowInstance {
    pub definition: WorkflowDefinition,
    #[serde(default)]
    pub context: serde_json::Value,
    pub current_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incoming_edge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_message: Option<String>,
    pub status: WorkflowStatus,
    #[serde(default)]
    pub max_rounds: WorkflowMaxRounds,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report: Option<WorkflowReport>,
    pub updated_at: DateTime<Utc>,
}

impl WorkflowDefinition {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("workflow version must be 1".into());
        }
        if self.id.trim().is_empty() {
            return Err("workflow id may not be empty".into());
        }
        if self.name.trim().is_empty() {
            return Err("workflow name may not be empty".into());
        }
        if self.prompt.trim().is_empty() || self.start_prompt.trim().is_empty() {
            return Err("workflow prompt and start_prompt may not be empty".into());
        }
        let mut node_ids = HashSet::new();
        for node in &self.nodes {
            if node.id.trim().is_empty() || node.prompt.trim().is_empty() {
                return Err("node id and prompt may not be empty".into());
            }
            if let Some(agent) = node.agent.as_deref() {
                validate_agent_id(agent)?;
            }
            if !node_ids.insert(node.id.as_str()) {
                return Err(format!("duplicate node id `{}`", node.id));
            }
        }
        if !node_ids.contains(self.initial_node.as_str()) {
            return Err("initial_node does not exist".into());
        }
        if !node_ids.contains(self.terminal_node.as_str()) {
            return Err("terminal_node does not exist".into());
        }
        let mut edge_ids = HashSet::new();
        let mut outgoing: HashMap<&str, usize> = HashMap::new();
        for edge in &self.edges {
            if edge.id.trim().is_empty() || edge.prompt.trim().is_empty() {
                return Err("edge id and prompt may not be empty".into());
            }
            if !edge_ids.insert(edge.id.as_str()) {
                return Err(format!("duplicate edge id `{}`", edge.id));
            }
            if !node_ids.contains(edge.from.as_str()) || !node_ids.contains(edge.to.as_str()) {
                return Err(format!("edge `{}` references unknown node", edge.id));
            }
            if edge.from == self.terminal_node {
                return Err("terminal node may not have outgoing edges".into());
            }
            *outgoing.entry(&edge.from).or_default() += 1;
        }
        for node in &self.nodes {
            if node.id != self.terminal_node
                && outgoing.get(node.id.as_str()).copied().unwrap_or(0) == 0
            {
                return Err(format!(
                    "non-terminal node `{}` has no outgoing edges",
                    node.id
                ));
            }
        }
        Ok(())
    }

    pub fn node(&self, id: &str) -> Option<&WorkflowNode> {
        self.nodes.iter().find(|node| node.id == id)
    }
    pub fn node_agent(&self, id: &str) -> Option<&str> {
        self.node(id)
            .and_then(|node| node.agent.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }
    pub fn edge(&self, id: &str) -> Option<&WorkflowEdge> {
        self.edges.iter().find(|edge| edge.id == id)
    }
    pub fn outgoing(&self, node: &str) -> Vec<&WorkflowEdge> {
        self.edges.iter().filter(|edge| edge.from == node).collect()
    }
}

fn validate_agent_id(agent: &str) -> Result<(), String> {
    let agent = agent.trim();
    if agent.is_empty() {
        return Err("node agent may not be empty".into());
    }
    if !agent
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(format!(
            "node agent `{agent}` may only contain ASCII letters, numbers, '-' and '_'"
        ));
    }
    Ok(())
}

pub fn embedded_goal_definition() -> WorkflowDefinition {
    serde_json::from_str(include_str!("../workflows/goal.json"))
        .expect("embedded goal workflow must be valid")
}

pub fn list_definitions(data_dir: &Path) -> Result<Vec<WorkflowDefinition>, String> {
    let mut definitions = vec![embedded_goal_definition()];
    let dir = data_dir.join("workflows");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(definitions),
        Err(err) => return Err(format!("reading {}: {err}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|err| format!("reading {}: {err}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("reading {}: {err}", path.display()))?;
        let definition: WorkflowDefinition = serde_json::from_str(&text)
            .map_err(|err| format!("parsing {}: {err}", path.display()))?;
        definition
            .validate()
            .map_err(|err| format!("invalid workflow {}: {err}", path.display()))?;
        if definition.id != id {
            return Err(format!(
                "workflow id `{}` does not match file name `{id}.json`",
                definition.id
            ));
        }
        definitions.push(definition);
    }
    definitions.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(definitions)
}

pub const USER_STATE_KEY: &str = "__supervisor_workflow";

pub fn instance_from_user_state(user_state: &serde_json::Value) -> Option<WorkflowInstance> {
    serde_json::from_value(user_state.get(USER_STATE_KEY)?.clone()).ok()
}

pub fn set_instance_in_user_state(
    user_state: &mut serde_json::Value,
    instance: &WorkflowInstance,
) -> Result<(), serde_json::Error> {
    if !user_state.is_object() {
        *user_state = serde_json::json!({});
    }
    if let Some(object) = user_state.as_object_mut() {
        object.insert(USER_STATE_KEY.to_string(), serde_json::to_value(instance)?);
    }
    Ok(())
}

pub fn remove_instance_from_user_state(user_state: &mut serde_json::Value) {
    if let Some(object) = user_state.as_object_mut() {
        object.remove(USER_STATE_KEY);
    }
}

pub fn instance_path(data_dir: &Path, thread_id: &str) -> PathBuf {
    data_dir
        .join("memory")
        .join(sanitize_id(thread_id))
        .join("supervisor_workflow.json")
}

pub async fn load_instance(data_dir: &Path, thread_id: &str) -> Option<WorkflowInstance> {
    let text = tokio::fs::read_to_string(instance_path(data_dir, thread_id))
        .await
        .ok()?;
    serde_json::from_str(&text).ok()
}

pub async fn save_instance(
    data_dir: &Path,
    thread_id: &str,
    instance: &WorkflowInstance,
) -> Result<(), std::io::Error> {
    let path = instance_path(data_dir, thread_id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, serde_json::to_vec_pretty(instance)?).await
}

pub async fn clear_instance(data_dir: &Path, thread_id: &str) -> Result<(), std::io::Error> {
    match tokio::fs::remove_file(instance_path(data_dir, thread_id)).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub async fn load_definition(data_dir: &Path, id: &str) -> Result<WorkflowDefinition, String> {
    if id == "goal" {
        return Ok(embedded_goal_definition());
    }
    if id.is_empty() || sanitize_id(id) != id {
        return Err("workflow id may only contain letters, numbers, '-' and '_'".into());
    }
    let path = data_dir
        .join("workflows")
        .join(format!("{}.json", sanitize_id(id)));
    let text = tokio::fs::read_to_string(&path)
        .await
        .map_err(|err| format!("reading {}: {err}", path.display()))?;
    let definition: WorkflowDefinition =
        serde_json::from_str(&text).map_err(|err| format!("parsing {}: {err}", path.display()))?;
    definition.validate()?;
    if definition.id != id {
        return Err(format!(
            "workflow id `{}` does not match file name `{id}.json`",
            definition.id
        ));
    }
    Ok(definition)
}

pub fn parse_decision(raw: &str) -> Result<WorkflowDecision, String> {
    let json = extract_json_object(raw.trim()).ok_or_else(|| "missing JSON object".to_string())?;
    let mut decision: WorkflowDecision =
        serde_json::from_str(json).map_err(|err| format!("invalid supervisor JSON: {err}"))?;
    decision.edge = decision.edge.trim().to_string();
    decision.agent_message = trim_option(decision.agent_message);
    decision.next_node_message = trim_option(decision.next_node_message);
    decision.reason = decision.reason.trim().to_string();
    if decision.edge.is_empty() {
        return Err("edge may not be empty".into());
    }
    if decision.reason.is_empty() {
        return Err("reason may not be empty".into());
    }
    Ok(decision)
}

pub fn apply_decision(
    instance: &mut WorkflowInstance,
    decision: WorkflowDecision,
    round: u32,
) -> Result<(WorkflowReport, Option<String>), String> {
    let from_node = instance.current_node.clone();
    let edge = instance
        .definition
        .edge(&decision.edge)
        .cloned()
        .ok_or_else(|| format!("edge `{}` does not exist", decision.edge))?;
    if edge.from != from_node {
        return Err(format!(
            "edge `{}` is not allowed from current node",
            edge.id
        ));
    }
    let terminal = edge.to == instance.definition.terminal_node;
    if !terminal && decision.agent_message.is_none() {
        return Err("non-terminal transition requires agent_message".into());
    }

    let agent_message = if terminal {
        None
    } else {
        decision.agent_message
    };
    let next_node_message = decision.next_node_message;
    instance.current_node = edge.to.clone();
    instance.incoming_edge = Some(edge.id.clone());
    instance.node_message = next_node_message.clone();
    instance.status = if terminal {
        WorkflowStatus::Completed
    } else {
        WorkflowStatus::Active
    };
    instance.updated_at = Utc::now();
    let report = WorkflowReport {
        workflow_id: instance.definition.id.clone(),
        workflow_name: instance.definition.name.clone(),
        from_node,
        edge: Some(edge.id),
        to_node: edge.to,
        status: instance.status.clone(),
        reason: decision.reason,
        agent_message: agent_message.clone(),
        next_node_message,
        supervisor_trace: decision.trace,
        round,
        max_rounds: instance.max_rounds.clone(),
        error: None,
    };
    instance.last_report = Some(report.clone());
    Ok((report, agent_message))
}

pub fn supervisor_prompt(
    instance: &WorkflowInstance,
    history: &[Message],
    todo_prompt: Option<&str>,
) -> Result<String, String> {
    let definition = &instance.definition;
    let node = definition
        .node(&instance.current_node)
        .ok_or_else(|| "current node is missing".to_string())?;
    let incoming = match instance.incoming_edge.as_deref() {
        Some(id) => {
            let edge = definition
                .edge(id)
                .ok_or_else(|| "incoming edge is missing".to_string())?;
            serde_json::json!({"id": edge.id, "prompt": edge.prompt})
        }
        None => serde_json::json!({"id": "__start__", "prompt": definition.start_prompt}),
    };
    let outgoing: Vec<_> = definition
        .outgoing(&node.id)
        .into_iter()
        .map(|edge| {
            let to_agent = definition.node_agent(&edge.to);
            serde_json::json!({"id": edge.id, "to": edge.to, "to_agent": to_agent, "prompt": edge.prompt})
        })
        .collect();
    let payload = serde_json::json!({
        "workflow_prompt": definition.prompt,
        "context": instance.context,
        "previous_node_message": instance.node_message,
        "incoming_edge": incoming,
        "current_node": {"id": node.id, "agent": node.agent, "prompt": node.prompt},
        "allowed_outgoing_edges": outgoing,
        "todo": todo_prompt,
        "main_agent_history": history,
    });
    Ok(format!(
        "You are a supervisor workflow agent. Use tools when needed to verify the main agent's work.\n\
         Select exactly one allowed outgoing edge. Return only JSON with this shape:\n\
         {{\"edge\":\"edge id\",\"agent_message\":\"required instruction for non-terminal transitions, otherwise null\",\"next_node_message\":\"optional private message for the next supervisor node\",\"reason\":\"short reason\"}}\n\
         For a transition to a non-terminal node, agent_message must be non-empty. For a terminal transition it is ignored.\n\n{}",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into())
    ))
}

pub fn format_prefix(report: &WorkflowReport, context: &serde_json::Value) -> String {
    if report.workflow_id == "goal" {
        let goal = context.get("goal").and_then(|v| v.as_str()).unwrap_or("");
        return format!(
            "\n\n**Goal**: {}\n\n**Supervisor**: {} - {}\n\n---\n\n",
            goal,
            report.edge.as_deref().unwrap_or("error"),
            report.reason
        );
    }
    format!(
        "\n\n**Workflow**: {}\n\n**Supervisor**: {} -> {} via {} - {}\n\n---\n\n",
        report.workflow_name,
        report.from_node,
        report.to_node,
        report.edge.as_deref().unwrap_or("error"),
        report.reason
    )
}

fn trim_option(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
fn extract_json_object(raw: &str) -> Option<&str> {
    let value = raw
        .strip_prefix("```json")
        .or_else(|| raw.strip_prefix("```"))
        .and_then(|rest| rest.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(raw);
    let start = value.find('{')?;
    let end = value.rfind('}')?;
    (start <= end).then(|| &value[start..=end])
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
    fn goal_graph_is_valid() {
        embedded_goal_definition().validate().unwrap();
    }

    #[test]
    fn lists_embedded_and_profile_workflows() {
        let data_dir = tempfile::tempdir().unwrap();
        let workflows_dir = data_dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let definition = WorkflowDefinition {
            version: 1,
            id: "verify".into(),
            name: "Verify Work".into(),
            description: "Check the work".into(),
            prompt: "supervise".into(),
            start_prompt: "start".into(),
            initial_node: "review".into(),
            terminal_node: "done".into(),
            nodes: vec![
                WorkflowNode {
                    id: "review".into(),
                    agent: None,
                    prompt: "review".into(),
                },
                WorkflowNode {
                    id: "done".into(),
                    agent: None,
                    prompt: "done".into(),
                },
            ],
            edges: vec![WorkflowEdge {
                id: "complete".into(),
                from: "review".into(),
                to: "done".into(),
                prompt: "complete".into(),
            }],
        };
        std::fs::write(
            workflows_dir.join("verify.json"),
            serde_json::to_string_pretty(&definition).unwrap(),
        )
        .unwrap();

        let definitions = list_definitions(data_dir.path()).unwrap();

        assert!(definitions.iter().any(|workflow| workflow.id == "goal"));
        assert!(definitions
            .iter()
            .any(|workflow| workflow.id == "verify" && workflow.name == "Verify Work"));
    }

    #[test]
    fn rejects_terminal_edges() {
        let mut graph = embedded_goal_definition();
        graph.edges.push(WorkflowEdge {
            id: "bad".into(),
            from: "stop".into(),
            to: "review".into(),
            prompt: "bad".into(),
        });
        assert!(graph.validate().unwrap_err().contains("terminal"));
    }

    #[test]
    fn workflow_node_agent_is_optional_and_validated() {
        let definition: WorkflowDefinition = serde_json::from_str(
            r#"{
                "version": 1,
                "id": "agent-workflow",
                "name": "Agent Workflow",
                "description": "",
                "prompt": "supervise",
                "start_prompt": "start",
                "initial_node": "explore",
                "terminal_node": "done",
                "nodes": [
                    {"id": "explore", "agent": "explorer", "prompt": "explore"},
                    {"id": "done", "prompt": "done"}
                ],
                "edges": [
                    {"id": "complete", "from": "explore", "to": "done", "prompt": "complete"}
                ]
            }"#,
        )
        .unwrap();

        definition.validate().unwrap();
        assert_eq!(definition.node_agent("explore"), Some("explorer"));
        assert_eq!(definition.node_agent("done"), None);
    }

    #[test]
    fn parses_workflow_decision() {
        let decision = parse_decision(
            r#"{"edge":"continue","agent_message":"work","next_node_message":null,"reason":"not done"}"#,
        )
        .unwrap();
        assert_eq!(decision.edge, "continue");
        assert_eq!(decision.agent_message.as_deref(), Some("work"));
    }

    #[test]
    fn prompt_contains_fixed_node_input() {
        let instance = WorkflowInstance {
            definition: embedded_goal_definition(),
            context: serde_json::json!({"goal": "ship it"}),
            current_node: "review".into(),
            incoming_edge: Some("continue".into()),
            node_message: Some("check the tests next".into()),
            status: WorkflowStatus::Active,
            max_rounds: WorkflowMaxRounds::default(),
            last_report: None,
            updated_at: Utc::now(),
        };

        let prompt = supervisor_prompt(
            &instance,
            &[],
            Some("[CURRENT TODO BATCH]\n- #1 check the tests"),
        )
        .unwrap();
        assert!(prompt.contains("check the tests next"));
        assert!(prompt.contains("[CURRENT TODO BATCH]"));
        assert!(prompt.contains("check the tests"));
        assert!(prompt.contains("Choose when more main-agent work is required."));
        assert!(prompt.contains("Review the complete main-agent history"));
        assert!(prompt.contains("\"main_agent_history\": []"));
    }

    #[test]
    fn applies_three_node_workflow_transitions() {
        let definition = WorkflowDefinition {
            version: 1,
            id: "verify".into(),
            name: "Verify".into(),
            description: String::new(),
            prompt: "supervise".into(),
            start_prompt: "start".into(),
            initial_node: "review".into(),
            terminal_node: "stop".into(),
            nodes: vec![
                WorkflowNode {
                    id: "review".into(),
                    agent: None,
                    prompt: "review".into(),
                },
                WorkflowNode {
                    id: "verify".into(),
                    agent: Some("explorer".into()),
                    prompt: "verify".into(),
                },
                WorkflowNode {
                    id: "stop".into(),
                    agent: None,
                    prompt: "stop".into(),
                },
            ],
            edges: vec![
                WorkflowEdge {
                    id: "verify".into(),
                    from: "review".into(),
                    to: "verify".into(),
                    prompt: "verify next".into(),
                },
                WorkflowEdge {
                    id: "complete".into(),
                    from: "verify".into(),
                    to: "stop".into(),
                    prompt: "complete".into(),
                },
            ],
        };
        let mut instance = WorkflowInstance {
            current_node: definition.initial_node.clone(),
            definition,
            context: serde_json::json!({}),
            incoming_edge: None,
            node_message: None,
            status: WorkflowStatus::Active,
            max_rounds: WorkflowMaxRounds::default(),
            last_report: None,
            updated_at: Utc::now(),
        };

        let (_, message) = apply_decision(
            &mut instance,
            WorkflowDecision {
                edge: "verify".into(),
                agent_message: Some("run tests".into()),
                next_node_message: Some("implementation looked complete".into()),
                reason: "needs verification".into(),
                trace: Vec::new(),
            },
            0,
        )
        .unwrap();
        assert_eq!(message.as_deref(), Some("run tests"));
        assert_eq!(instance.current_node, "verify");
        assert_eq!(instance.incoming_edge.as_deref(), Some("verify"));
        assert_eq!(
            instance.node_message.as_deref(),
            Some("implementation looked complete")
        );

        let (report, message) = apply_decision(
            &mut instance,
            WorkflowDecision {
                edge: "complete".into(),
                agent_message: Some("this must be ignored".into()),
                next_node_message: None,
                reason: "verified".into(),
                trace: Vec::new(),
            },
            1,
        )
        .unwrap();
        assert_eq!(message, None);
        assert_eq!(report.status, WorkflowStatus::Completed);
        assert_eq!(instance.current_node, "stop");
    }
}
