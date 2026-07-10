use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use remi_agentloop::prelude::Message;
use serde::{Deserialize, Serialize};

use crate::estimate_model_input_tokens;

pub const ASK_USER_FOR_HELP_NODE: &str = "ask_user_for_help";

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPauseReason {
    Manual,
    WaitingForUserInput,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AskUserForHelpState {
    pub return_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_incoming_edge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_node_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDecision {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_node: Option<String>,
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
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        id: String,
        delta: String,
    },
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        id: String,
        name: String,
        args: serde_json::Value,
        result: String,
    },
    OutputDelta {
        content: String,
    },
    Output {
        content: String,
    },
    AgentMessage {
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_edges: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_nodes: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_reason: Option<WorkflowPauseReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_user_for_help: Option<AskUserForHelpState>,
    #[serde(default)]
    pub max_rounds: WorkflowMaxRounds,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report: Option<WorkflowReport>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorPrompt {
    pub content: String,
    pub estimated_tokens: u32,
    pub budget_tokens: Option<u32>,
    pub original_history_messages: usize,
    pub included_history_messages: usize,
}

impl WorkflowInstance {
    pub fn enter_ask_user_for_help(&mut self, round: u32) -> WorkflowReport {
        let from_node = self.current_node.clone();
        self.ask_user_for_help = Some(AskUserForHelpState {
            return_node: from_node.clone(),
            return_incoming_edge: self.incoming_edge.clone(),
            return_node_message: self.node_message.clone(),
        });
        self.current_node = ASK_USER_FOR_HELP_NODE.to_string();
        self.incoming_edge = Some(ASK_USER_FOR_HELP_NODE.to_string());
        self.node_message = Some("Waiting for user input.".to_string());
        self.status = WorkflowStatus::Paused;
        self.pause_reason = Some(WorkflowPauseReason::WaitingForUserInput);
        self.updated_at = Utc::now();

        let report = WorkflowReport {
            workflow_id: self.definition.id.clone(),
            workflow_name: self.definition.name.clone(),
            from_node: from_node.clone(),
            edge: Some(ASK_USER_FOR_HELP_NODE.to_string()),
            to_node: ASK_USER_FOR_HELP_NODE.to_string(),
            path_edges: vec![ASK_USER_FOR_HELP_NODE.to_string()],
            path_nodes: vec![from_node, ASK_USER_FOR_HELP_NODE.to_string()],
            status: WorkflowStatus::Paused,
            reason: "waiting for user input".to_string(),
            agent_message: None,
            next_node_message: self.node_message.clone(),
            supervisor_trace: Vec::new(),
            round,
            max_rounds: self.max_rounds.clone(),
            error: None,
        };
        self.last_report = Some(report.clone());
        report
    }

    pub fn resume_from_ask_user_for_help(&mut self, round: u32) -> Option<WorkflowReport> {
        if self.status != WorkflowStatus::Paused
            || self.pause_reason != Some(WorkflowPauseReason::WaitingForUserInput)
            || self.current_node != ASK_USER_FOR_HELP_NODE
        {
            return None;
        }
        let ask_state = self.ask_user_for_help.take()?;
        let from_node = self.current_node.clone();
        self.current_node = ask_state.return_node;
        self.incoming_edge = ask_state.return_incoming_edge;
        self.node_message = ask_state.return_node_message;
        self.status = WorkflowStatus::Active;
        self.pause_reason = None;
        self.updated_at = Utc::now();

        let resume_edge = format!("{ASK_USER_FOR_HELP_NODE}_resume");
        let report = WorkflowReport {
            workflow_id: self.definition.id.clone(),
            workflow_name: self.definition.name.clone(),
            from_node,
            edge: Some(resume_edge.clone()),
            to_node: self.current_node.clone(),
            path_edges: vec![resume_edge],
            path_nodes: vec![
                ASK_USER_FOR_HELP_NODE.to_string(),
                self.current_node.clone(),
            ],
            status: WorkflowStatus::Active,
            reason: "user answered; resuming workflow".to_string(),
            agent_message: None,
            next_node_message: self.node_message.clone(),
            supervisor_trace: Vec::new(),
            round,
            max_rounds: self.max_rounds.clone(),
            error: None,
        };
        self.last_report = Some(report.clone());
        Some(report)
    }
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

    pub fn reachable_subtree(&self, root: &str) -> Result<WorkflowSubtreeNode, String> {
        let mut seen = HashSet::new();
        seen.insert(root.to_string());
        self.build_subtree_node(root, None, vec![root.to_string()], Vec::new(), &mut seen)
    }

    pub fn jump_targets(&self, root: &str) -> Result<Vec<WorkflowJumpTarget>, String> {
        let subtree = self.reachable_subtree(root)?;
        let mut targets = Vec::new();
        collect_jump_targets(root, &subtree, &mut targets);
        Ok(targets)
    }

    pub fn descendant_path(&self, root: &str, target: &str) -> Option<Vec<WorkflowEdge>> {
        if root == target {
            return None;
        }
        let mut seen = HashSet::new();
        seen.insert(root.to_string());
        self.descendant_path_inner(root, target, &mut seen)
    }

    fn descendant_path_inner(
        &self,
        node: &str,
        target: &str,
        seen: &mut HashSet<String>,
    ) -> Option<Vec<WorkflowEdge>> {
        for edge in self.outgoing(node) {
            if edge.to == target {
                return Some(vec![edge.clone()]);
            }
            if seen.contains(&edge.to) {
                continue;
            }
            seen.insert(edge.to.clone());
            if let Some(mut path) = self.descendant_path_inner(&edge.to, target, seen) {
                let mut edges = vec![edge.clone()];
                edges.append(&mut path);
                return Some(edges);
            }
            seen.remove(&edge.to);
        }
        None
    }

    fn build_subtree_node(
        &self,
        node_id: &str,
        via_edge: Option<WorkflowSubtreeEdge>,
        path_nodes: Vec<String>,
        path_edges: Vec<String>,
        seen: &mut HashSet<String>,
    ) -> Result<WorkflowSubtreeNode, String> {
        let node = self
            .node(node_id)
            .ok_or_else(|| format!("node `{node_id}` does not exist"))?;
        let cycle_cut = via_edge.is_some() && path_nodes[..path_nodes.len() - 1].contains(&node.id);
        let jump_allowed = via_edge.is_some() && node.id != path_nodes[0];
        let mut subtree = WorkflowSubtreeNode {
            id: node.id.clone(),
            agent: node.agent.clone(),
            prompt: node.prompt.clone(),
            via_edge,
            path_nodes,
            path_edges,
            cycle_cut,
            jump_allowed,
            children: Vec::new(),
        };
        if cycle_cut {
            return Ok(subtree);
        }
        for edge in self.outgoing(&node.id) {
            let mut child_path_nodes = subtree.path_nodes.clone();
            child_path_nodes.push(edge.to.clone());
            let mut child_path_edges = subtree.path_edges.clone();
            child_path_edges.push(edge.id.clone());
            let child_via_edge = Some(WorkflowSubtreeEdge {
                id: edge.id.clone(),
                prompt: edge.prompt.clone(),
            });
            let repeats = seen.contains(&edge.to);
            if !repeats {
                seen.insert(edge.to.clone());
            }
            let child = self.build_subtree_node(
                &edge.to,
                child_via_edge,
                child_path_nodes,
                child_path_edges,
                seen,
            )?;
            if !repeats {
                seen.remove(&edge.to);
            }
            subtree.children.push(child);
        }
        Ok(subtree)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowSubtreeNode {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_edge: Option<WorkflowSubtreeEdge>,
    pub path_nodes: Vec<String>,
    pub path_edges: Vec<String>,
    pub cycle_cut: bool,
    pub jump_allowed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<WorkflowSubtreeNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowSubtreeEdge {
    pub id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowJumpTarget {
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_agent: Option<String>,
    pub node_prompt: String,
    pub path_nodes: Vec<String>,
    pub path_edges: Vec<String>,
    pub cycle_cut: bool,
}

fn collect_jump_targets(
    root: &str,
    node: &WorkflowSubtreeNode,
    targets: &mut Vec<WorkflowJumpTarget>,
) {
    if node.jump_allowed && node.id != root {
        targets.push(WorkflowJumpTarget {
            node_id: node.id.clone(),
            node_agent: node.agent.clone(),
            node_prompt: node.prompt.clone(),
            path_nodes: node.path_nodes.clone(),
            path_edges: node.path_edges.clone(),
            cycle_cut: node.cycle_cut,
        });
    }
    for child in &node.children {
        collect_jump_targets(root, child, targets);
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
    decision.edge = trim_option(decision.edge);
    decision.target_node = trim_option(decision.target_node);
    decision.agent_message = trim_option(decision.agent_message);
    decision.next_node_message = trim_option(decision.next_node_message);
    decision.reason = decision.reason.trim().to_string();
    if decision.edge.is_none() && decision.target_node.is_none() {
        return Err("edge or target_node is required".into());
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
    let path_edges = if let Some(target_node) = decision.target_node.as_deref() {
        if target_node == from_node {
            let agent_message = decision
                .agent_message
                .ok_or_else(|| "current-node continuation requires agent_message".to_string())?;
            instance.status = WorkflowStatus::Active;
            instance.node_message = decision.next_node_message.clone();
            instance.updated_at = Utc::now();
            let report = WorkflowReport {
                workflow_id: instance.definition.id.clone(),
                workflow_name: instance.definition.name.clone(),
                from_node: from_node.clone(),
                edge: decision.edge,
                to_node: from_node.clone(),
                path_edges: Vec::new(),
                path_nodes: vec![from_node],
                status: instance.status.clone(),
                reason: decision.reason,
                agent_message: Some(agent_message.clone()),
                next_node_message: instance.node_message.clone(),
                supervisor_trace: decision.trace,
                round,
                max_rounds: instance.max_rounds.clone(),
                error: None,
            };
            instance.last_report = Some(report.clone());
            return Ok((report, Some(agent_message)));
        }
        let targets = instance
            .definition
            .jump_targets(&from_node)?
            .into_iter()
            .filter(|target| target.node_id == target_node)
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Err(format!(
                "target_node `{target_node}` is not reachable from current node `{from_node}`"
            ));
        }
        let selected = if let Some(edge_id) = decision.edge.as_deref() {
            targets
                .iter()
                .find(|target| target.path_edges.iter().any(|id| id == edge_id))
                .ok_or_else(|| {
                    format!(
                        "edge `{edge_id}` is not on the path from `{from_node}` to `{target_node}`"
                    )
                })?
        } else {
            &targets[0]
        };
        selected
            .path_edges
            .iter()
            .map(|edge_id| {
                instance.definition.edge(edge_id).cloned().ok_or_else(|| {
                    format!(
                        "edge `{edge_id}` on path from `{from_node}` to `{target_node}` does not exist"
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let edge_id = decision
            .edge
            .as_deref()
            .ok_or_else(|| "edge or target_node is required".to_string())?;
        let edge = instance
            .definition
            .edge(edge_id)
            .cloned()
            .ok_or_else(|| format!("edge `{edge_id}` does not exist"))?;
        if edge.from != from_node {
            return Err(format!(
                "edge `{}` is not allowed from current node",
                edge.id
            ));
        }
        vec![edge]
    };
    let edge = path_edges
        .last()
        .cloned()
        .ok_or_else(|| "workflow transition path may not be empty".to_string())?;
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
    let path_edge_ids: Vec<String> = path_edges.iter().map(|edge| edge.id.clone()).collect();
    let mut path_node_ids = vec![from_node.clone()];
    path_node_ids.extend(path_edges.iter().map(|edge| edge.to.clone()));
    let report = WorkflowReport {
        workflow_id: instance.definition.id.clone(),
        workflow_name: instance.definition.name.clone(),
        from_node,
        edge: Some(edge.id),
        to_node: edge.to,
        path_edges: path_edge_ids,
        path_nodes: path_node_ids,
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
    Ok(supervisor_prompt_with_budget(instance, history, todo_prompt, None)?.content)
}

pub fn supervisor_prompt_with_budget(
    instance: &WorkflowInstance,
    history: &[Message],
    todo_prompt: Option<&str>,
    budget_tokens: Option<u32>,
) -> Result<SupervisorPrompt, String> {
    let original_history_messages = history.len();
    let mut history = history.to_vec();
    loop {
        let content = render_supervisor_prompt(instance, &history, todo_prompt)?;
        let estimated_tokens = estimate_model_input_tokens(&content);
        let within_budget = budget_tokens.is_none_or(|budget| estimated_tokens <= budget);
        if within_budget || history.len() <= 1 {
            return Ok(SupervisorPrompt {
                content,
                estimated_tokens,
                budget_tokens,
                original_history_messages,
                included_history_messages: history.len(),
            });
        }
        history.remove(0);
    }
}

fn render_supervisor_prompt(
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
    let allowed_subtree = definition.reachable_subtree(&node.id)?;
    let allowed_jump_targets = definition.jump_targets(&node.id)?;
    let payload = serde_json::json!({
        "workflow_prompt": definition.prompt,
        "context": instance.context,
        "previous_node_message": instance.node_message,
        "incoming_edge": incoming,
        "current_node": {"id": node.id, "agent": node.agent, "prompt": node.prompt},
        "allowed_outgoing_edges": outgoing,
        "allowed_subtree": allowed_subtree,
        "allowed_jump_targets": allowed_jump_targets,
        "todo": todo_prompt,
        "main_agent_history": history,
    });
    Ok(format!(
        "You are a supervisor workflow agent. You have no tools; make decisions from the workflow payload and main agent history.\n\
         Do not overthink: make the shortest sound decision from the available evidence and keep reasoning focused.\n\
         Select either a direct edge from allowed_outgoing_edges or any target_node from allowed_jump_targets. Use target_node when the main agent appears to have already completed intermediate workflow nodes.\n\
         allowed_subtree is rooted at the current node; cycle_cut=true means the subtree was truncated at a repeated node and must not be expanded further.\n\
         Return only JSON with this shape, using the JSON null literal for absent optional fields, never the string \"null\":\n\
         {{\"target_node\":null,\"edge\":\"direct edge id\",\"agent_message\":\"required instruction for non-terminal transitions, otherwise null\",\"next_node_message\":null,\"reason\":\"short reason\"}}\n\
         For a transition to a non-terminal node, agent_message must be non-empty. For a terminal transition it is ignored.\n\
         agent_message must tell the main agent the current node goal to execute now, summarize what later workflow nodes or phases may still follow based on allowed_outgoing_edges and allowed_subtree, and explicitly instruct the main agent to complete only the current node goal without performing later-node goals early.\n\n{}",
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
    value.map(|v| v.trim().to_string()).filter(|v| {
        !v.is_empty()
            && !matches!(
                v.to_ascii_lowercase().as_str(),
                "null" | "none" | "nil" | "n/a"
            )
    })
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
        assert_eq!(decision.edge.as_deref(), Some("continue"));
        assert_eq!(decision.agent_message.as_deref(), Some("work"));
    }

    #[test]
    fn parses_target_node_decision() {
        let decision = parse_decision(
            r#"{"target_node":" verify ","agent_message":"work","next_node_message":null,"reason":" skipped ahead "}"#,
        )
        .unwrap();
        assert_eq!(decision.edge, None);
        assert_eq!(decision.target_node.as_deref(), Some("verify"));
        assert_eq!(decision.reason, "skipped ahead");
    }

    #[test]
    fn parses_string_null_optional_fields_as_absent() {
        let decision = parse_decision(
            r#"{"target_node":"null","edge":"continue","agent_message":"work","next_node_message":" null ","reason":"not done"}"#,
        )
        .unwrap();

        assert_eq!(decision.target_node, None);
        assert_eq!(decision.edge.as_deref(), Some("continue"));
        assert_eq!(decision.next_node_message, None);
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
            pause_reason: None,
            ask_user_for_help: None,
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
        assert!(prompt.contains("You have no tools"));
        assert!(!prompt.contains("Use tools when needed"));
        assert!(prompt.contains("Choose when more main-agent work is required."));
        assert!(prompt.contains("Review the complete main-agent history"));
        assert!(prompt.contains("\"allowed_subtree\""));
        assert!(prompt.contains("\"allowed_jump_targets\""));
        assert!(prompt.contains("\"main_agent_history\": []"));
        assert!(prompt.contains("current node goal to execute now"));
        assert!(prompt.contains("later workflow nodes or phases may still follow"));
        assert!(prompt.contains("complete only the current node goal"));
        assert!(prompt.contains("without performing later-node goals early"));
    }

    #[test]
    fn prompt_budget_trims_old_history_messages_first() {
        let instance = WorkflowInstance {
            definition: embedded_goal_definition(),
            context: serde_json::json!({"goal": "ship it"}),
            current_node: "review".into(),
            incoming_edge: None,
            node_message: None,
            status: WorkflowStatus::Active,
            pause_reason: None,
            ask_user_for_help: None,
            max_rounds: WorkflowMaxRounds::default(),
            last_report: None,
            updated_at: Utc::now(),
        };
        let history = vec![
            Message::user(format!("old session message {}", "a".repeat(2_000))),
            Message::assistant(format!("middle session message {}", "b".repeat(2_000))),
            Message::user("latest session message".to_string()),
        ];
        let latest_only = supervisor_prompt(&instance, &history[2..], None).unwrap();
        let budget = estimate_model_input_tokens(&latest_only).saturating_add(10);

        let prompt =
            supervisor_prompt_with_budget(&instance, &history, None, Some(budget)).unwrap();

        assert!(prompt.included_history_messages < prompt.original_history_messages);
        assert!(prompt.content.contains("latest session message"));
        assert!(!prompt.content.contains("old session message"));
        assert!(prompt.estimated_tokens <= budget);
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
            pause_reason: None,
            ask_user_for_help: None,
            max_rounds: WorkflowMaxRounds::default(),
            last_report: None,
            updated_at: Utc::now(),
        };

        let (_, message) = apply_decision(
            &mut instance,
            WorkflowDecision {
                edge: Some("verify".into()),
                target_node: None,
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
                edge: Some("complete".into()),
                target_node: None,
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

    #[test]
    fn builds_cycle_truncated_subtree() {
        let definition = cyclic_definition();

        let subtree = definition.reachable_subtree("a").unwrap();

        assert_eq!(subtree.id, "a");
        let b = &subtree.children[0];
        let c = &b.children[0];
        let repeated_a = &c.children[0];
        assert_eq!(b.id, "b");
        assert_eq!(c.id, "c");
        assert_eq!(repeated_a.id, "a");
        assert!(repeated_a.cycle_cut);
        assert!(repeated_a.children.is_empty());
        assert!(!repeated_a.jump_allowed);

        let targets = definition.jump_targets("a").unwrap();
        let target_ids: Vec<_> = targets
            .iter()
            .map(|target| target.node_id.as_str())
            .collect();
        assert_eq!(target_ids, vec!["b", "c"]);
    }

    #[test]
    fn applies_descendant_target_jump() {
        let mut instance = workflow_instance(linear_definition());

        let (report, message) = apply_decision(
            &mut instance,
            WorkflowDecision {
                edge: None,
                target_node: Some("verify".into()),
                agent_message: Some("verify what already changed".into()),
                next_node_message: Some("skipped implementation".into()),
                reason: "main agent already implemented".into(),
                trace: Vec::new(),
            },
            0,
        )
        .unwrap();

        assert_eq!(message.as_deref(), Some("verify what already changed"));
        assert_eq!(instance.current_node, "verify");
        assert_eq!(instance.incoming_edge.as_deref(), Some("to_verify"));
        assert_eq!(report.edge.as_deref(), Some("to_verify"));
        assert_eq!(report.path_edges, vec!["to_implement", "to_verify"]);
        assert_eq!(report.path_nodes, vec!["review", "implement", "verify"]);
    }

    #[test]
    fn current_target_node_continues_current_node_without_error() {
        let mut instance = workflow_instance(embedded_goal_definition());
        instance.current_node = "review".into();

        let (report, message) = apply_decision(
            &mut instance,
            WorkflowDecision {
                edge: Some("continue".into()),
                target_node: Some("review".into()),
                agent_message: Some("try the requested tools again".into()),
                next_node_message: None,
                reason: "goal still needs current node work".into(),
                trace: Vec::new(),
            },
            1,
        )
        .unwrap();

        assert_eq!(message.as_deref(), Some("try the requested tools again"));
        assert_eq!(instance.current_node, "review");
        assert_eq!(instance.status, WorkflowStatus::Active);
        assert_eq!(report.status, WorkflowStatus::Active);
        assert_eq!(report.to_node, "review");
        assert_eq!(report.path_nodes, vec!["review"]);
        assert!(report.path_edges.is_empty());
    }

    #[test]
    fn string_null_target_does_not_override_valid_edge() {
        let mut instance = workflow_instance(embedded_goal_definition());
        instance.current_node = "review".into();
        let decision = parse_decision(
            r#"{"target_node":"null","edge":"continue","agent_message":"keep working","next_node_message":"null","reason":"more work"}"#,
        )
        .unwrap();

        let (report, message) = apply_decision(&mut instance, decision, 1).unwrap();

        assert_eq!(message.as_deref(), Some("keep working"));
        assert_eq!(report.edge.as_deref(), Some("continue"));
        assert_eq!(report.to_node, "review");
        assert_eq!(instance.node_message, None);
    }

    #[test]
    fn ask_user_for_help_pauses_and_resumes_to_original_node() {
        let mut instance = workflow_instance(embedded_goal_definition());
        instance.current_node = "review".into();
        instance.incoming_edge = Some("continue".into());
        instance.node_message = Some("keep checking".into());

        let pause_report = instance.enter_ask_user_for_help(2);

        assert_eq!(pause_report.from_node, "review");
        assert_eq!(pause_report.to_node, ASK_USER_FOR_HELP_NODE);
        assert_eq!(instance.current_node, ASK_USER_FOR_HELP_NODE);
        assert_eq!(instance.status, WorkflowStatus::Paused);
        assert_eq!(
            instance.pause_reason,
            Some(WorkflowPauseReason::WaitingForUserInput)
        );

        let resume_report = instance.resume_from_ask_user_for_help(2).unwrap();

        assert_eq!(resume_report.from_node, ASK_USER_FOR_HELP_NODE);
        assert_eq!(resume_report.to_node, "review");
        assert_eq!(instance.current_node, "review");
        assert_eq!(instance.incoming_edge.as_deref(), Some("continue"));
        assert_eq!(instance.node_message.as_deref(), Some("keep checking"));
        assert_eq!(instance.status, WorkflowStatus::Active);
        assert_eq!(instance.pause_reason, None);
        assert!(instance.ask_user_for_help.is_none());
    }

    #[test]
    fn rejects_non_descendant_target() {
        let mut instance = workflow_instance(linear_definition());

        let err = apply_decision(
            &mut instance,
            WorkflowDecision {
                edge: None,
                target_node: Some("missing".into()),
                agent_message: Some("work".into()),
                next_node_message: None,
                reason: "bad target".into(),
                trace: Vec::new(),
            },
            0,
        )
        .unwrap_err();

        assert!(err.contains("not reachable"));
    }

    #[test]
    fn terminal_descendant_target_completes() {
        let mut instance = workflow_instance(linear_definition());

        let (report, message) = apply_decision(
            &mut instance,
            WorkflowDecision {
                edge: None,
                target_node: Some("stop".into()),
                agent_message: Some("ignored".into()),
                next_node_message: None,
                reason: "all done".into(),
                trace: Vec::new(),
            },
            0,
        )
        .unwrap();

        assert_eq!(message, None);
        assert_eq!(instance.status, WorkflowStatus::Completed);
        assert_eq!(report.to_node, "stop");
        assert_eq!(
            report.path_edges,
            vec!["to_implement", "to_verify", "complete"]
        );
    }

    fn workflow_instance(definition: WorkflowDefinition) -> WorkflowInstance {
        WorkflowInstance {
            current_node: definition.initial_node.clone(),
            definition,
            context: serde_json::json!({}),
            incoming_edge: None,
            node_message: None,
            status: WorkflowStatus::Active,
            pause_reason: None,
            ask_user_for_help: None,
            max_rounds: WorkflowMaxRounds::default(),
            last_report: None,
            updated_at: Utc::now(),
        }
    }

    fn linear_definition() -> WorkflowDefinition {
        WorkflowDefinition {
            version: 1,
            id: "linear".into(),
            name: "Linear".into(),
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
                    id: "implement".into(),
                    agent: None,
                    prompt: "implement".into(),
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
                    id: "to_implement".into(),
                    from: "review".into(),
                    to: "implement".into(),
                    prompt: "implement".into(),
                },
                WorkflowEdge {
                    id: "to_verify".into(),
                    from: "implement".into(),
                    to: "verify".into(),
                    prompt: "verify".into(),
                },
                WorkflowEdge {
                    id: "complete".into(),
                    from: "verify".into(),
                    to: "stop".into(),
                    prompt: "complete".into(),
                },
            ],
        }
    }

    fn cyclic_definition() -> WorkflowDefinition {
        WorkflowDefinition {
            version: 1,
            id: "cyclic".into(),
            name: "Cyclic".into(),
            description: String::new(),
            prompt: "supervise".into(),
            start_prompt: "start".into(),
            initial_node: "a".into(),
            terminal_node: "stop".into(),
            nodes: vec![
                WorkflowNode {
                    id: "a".into(),
                    agent: None,
                    prompt: "a".into(),
                },
                WorkflowNode {
                    id: "b".into(),
                    agent: None,
                    prompt: "b".into(),
                },
                WorkflowNode {
                    id: "c".into(),
                    agent: None,
                    prompt: "c".into(),
                },
                WorkflowNode {
                    id: "stop".into(),
                    agent: None,
                    prompt: "stop".into(),
                },
            ],
            edges: vec![
                WorkflowEdge {
                    id: "ab".into(),
                    from: "a".into(),
                    to: "b".into(),
                    prompt: "a to b".into(),
                },
                WorkflowEdge {
                    id: "bc".into(),
                    from: "b".into(),
                    to: "c".into(),
                    prompt: "b to c".into(),
                },
                WorkflowEdge {
                    id: "ca".into(),
                    from: "c".into(),
                    to: "a".into(),
                    prompt: "c to a".into(),
                },
            ],
        }
    }
}
