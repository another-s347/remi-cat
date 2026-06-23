use remi_agentloop::prelude::{AgentError, Message};
use remi_agentloop::types::SubSessionEvent;

use crate::approval::{ToolApprovalDecision, ToolApprovalRequest};
use crate::supervisor_workflow::{SupervisorTraceEvent, WorkflowReport};
use crate::user_question::{UserQuestionRequest, UserQuestionResponse};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactionStatus {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactionSource {
    Auto,
    Manual,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextCompactionEvent {
    pub id: String,
    pub thread_id: String,
    pub status: ContextCompactionStatus,
    pub source: ContextCompactionSource,
    pub compacted_messages: usize,
    pub remaining_messages: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInputSegmentCategory {
    SystemPrompt,
    SkillInjection,
    History,
    ToolInput,
    ToolOutput,
    CurrentUser,
    Metadata,
    UserState,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelInputSegment {
    pub id: String,
    pub category: ModelInputSegmentCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub title: String,
    pub content: String,
    pub token_estimate: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelInputTotals {
    pub estimated_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelInputSnapshot {
    pub run_id: String,
    pub thread_id: String,
    pub model_profile_id: String,
    pub model: String,
    pub created_at: String,
    pub segments: Vec<ModelInputSegment>,
    pub totals: ModelInputTotals,
}

impl ModelInputSnapshot {
    pub fn apply_usage(
        &mut self,
        prompt_tokens: u32,
        completion_tokens: u32,
        max_prompt_tokens: u32,
        context_tokens: u32,
    ) {
        self.totals.prompt_tokens = Some(prompt_tokens);
        self.totals.completion_tokens = Some(completion_tokens);
        self.totals.total_tokens = Some(prompt_tokens.saturating_add(completion_tokens));
        self.totals.max_prompt_tokens = Some(max_prompt_tokens);
        self.totals.context_tokens = Some(context_tokens);
    }
}

// ── Skill events ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum SkillEvent {
    Saved { name: String, path: String },
    Deleted { name: String },
}

// ── Todo events ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum TodoEvent {
    Added { id: u64, content: String },
    Completed { id: u64 },
    Updated { id: u64, content: String },
    Removed { id: u64 },
}

// ── Trigger events ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum TriggerEvent {
    Upserted {
        id: u64,
        name: String,
        enabled: bool,
    },
    Deleted {
        id: u64,
    },
}

// ── Top-level CatEvent ───────────────────────────────────────────────────────

/// All events emitted by `CatBot::stream()`.
#[derive(Debug, Clone)]
pub enum CatEvent {
    /// Streaming text delta from the assistant.
    Text(String),
    /// Full message history + user_state captured at run completion.
    History(Vec<Message>, serde_json::Value),
    /// A skill tool mutated the skill store.
    Skill(SkillEvent),
    /// A todo tool mutated the todo list.
    Todo(TodoEvent),
    /// A trigger tool mutated thread trigger state.
    Trigger(TriggerEvent),
    /// A nested ACP or sub-agent session emitted observable progress.
    SubSession(SubSessionEvent),
    /// Supervisor evaluated the active workflow after a main-agent round.
    Supervisor(WorkflowReport),
    /// Live progress emitted while the supervisor evaluates a workflow node.
    SupervisorProgress(SupervisorTraceEvent),
    /// Short-term context was compacted into memory.
    ContextCompaction(ContextCompactionEvent),
    /// Full model input snapshot captured before the request is sent.
    ModelInputSnapshot(ModelInputSnapshot),
    /// Run completed normally.
    Done,
    /// An error occurred (run aborted).
    Error(AgentError),
    /// Model thinking/reasoning content (from extended thinking).
    Thinking(String),
    /// A tool call has started streaming from the model.
    ToolCallStart { id: String, name: String },
    /// Streaming JSON argument text for a tool call.
    ToolCallArgumentsDelta { id: String, delta: String },
    /// A tool is being called (name + JSON arguments).
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    /// A risky tool call is waiting for approval.
    ToolApprovalRequested(ToolApprovalRequest),
    /// A tool approval request was updated, for example after risk review.
    ToolApprovalUpdated(ToolApprovalRequest),
    /// A tool approval request was resolved by a user or policy.
    ToolApprovalResolved {
        request: ToolApprovalRequest,
        decision: ToolApprovalDecision,
    },
    /// A tool call is waiting for a user answer.
    UserQuestionRequested(UserQuestionRequest),
    /// A pending user question was updated.
    UserQuestionUpdated(UserQuestionRequest),
    /// A user question was answered or cancelled.
    UserQuestionResolved {
        request: UserQuestionRequest,
        response: UserQuestionResponse,
    },
    /// A tool returned its result.
    ToolCallResult {
        id: String,
        name: String,
        args: serde_json::Value,
        result: String,
        success: bool,
        elapsed_ms: u64,
    },
    /// Token usage + elapsed time stats for the completed run.
    Stats {
        prompt_tokens: u32,
        completion_tokens: u32,
        max_prompt_tokens: u32,
        elapsed_ms: u64,
    },
    /// Intermediate state snapshot — user_state after each tool round.
    /// Used by `CatBot` to persist user_state eagerly.
    StateUpdate(serde_json::Value),
}
