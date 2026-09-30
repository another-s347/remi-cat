use bot_runtime_core::{AgentError, Message, SubSessionEvent};

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
    Agent,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextCompactionEvent {
    pub id: String,
    pub thread_id: String,
    pub status: ContextCompactionStatus,
    pub source: ContextCompactionSource,
    pub compacted_messages: usize,
    pub remaining_messages: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_tokens: Option<u32>,
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
    ToolDefinition,
    ProtocolOverhead,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SkillEvent {
    Saved { name: String, path: String },
    Deleted { name: String },
}

// ── Todo events ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TodoEvent {
    Added { id: u64, content: String },
    Completed { id: u64 },
    Updated { id: u64, content: String },
    Removed { id: u64 },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SteerQueuedEvent {
    pub steer_id: String,
    pub session_id: String,
    pub preview: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SteerInjectedEvent {
    pub steer_ids: Vec<String>,
    pub session_id: String,
    pub preview: String,
    pub count: usize,
    /// Source IM message to anchor a new channel reply when this user steer runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default)]
    pub next_turn: bool,
}

impl SteerInjectedEvent {
    pub fn from_batch(batch: &bot_runtime_core::CoreSteerBatch, session_id: String) -> Self {
        Self {
            steer_ids: batch.ids.clone(),
            session_id,
            preview: batch.preview.clone(),
            count: batch.count,
            message_id: batch
                .message_metadata
                .as_ref()
                .and_then(|metadata| metadata.get("message_id"))
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned),
            next_turn: batch.has_user_next_turn(),
        }
    }
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
    /// A nested ACP or sub-agent session emitted observable progress.
    SubSession(SubSessionEvent),
    /// The foreground agent has completed its model round and the turn is
    /// waiting only for background tool tasks.
    BackgroundTasksWaiting { count: usize },
    /// Supervisor evaluated the active workflow after a main-agent round.
    Supervisor(WorkflowReport),
    /// Live progress emitted while the supervisor evaluates a workflow node.
    SupervisorProgress(SupervisorTraceEvent),
    /// Short-term context was compacted into memory.
    ContextCompaction(ContextCompactionEvent),
    /// Full model input snapshot captured before the request is sent.
    ModelInputSnapshot(ModelInputSnapshot),
    /// User input was accepted into the active run's steer queue.
    SteerQueued(SteerQueuedEvent),
    /// Queued steer input was injected into the active model run.
    SteerInjected(SteerInjectedEvent),
    /// Run completed normally.
    Done,
    /// Run was cooperatively cancelled.
    Cancelled,
    /// A user action intentionally interrupted the run without treating it as an error.
    UserInterrupted { reason: String },
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
    /// A background tool task completed after the foreground tool call timed out.
    ToolTaskCompleted(crate::tool_tasks::ToolTaskRecord),
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

#[cfg(test)]
mod steer_tests {
    use bot_runtime_core::{Content, CoreSteerBatch, CoreSteerSource};

    use super::SteerInjectedEvent;

    #[test]
    fn injected_event_carries_the_source_message_anchor() {
        let batch = CoreSteerBatch {
            ids: vec!["steer-1".into()],
            content: Content::text("follow-up"),
            preview: "follow-up".into(),
            count: 1,
            message_metadata: Some(serde_json::json!({"message_id": "om_steer"})),
            user_name: None,
            sources: vec![CoreSteerSource::User],
        };
        let event = SteerInjectedEvent::from_batch(&batch, "session".into());
        assert_eq!(event.message_id.as_deref(), Some("om_steer"));
        assert_eq!(event.steer_ids, vec!["steer-1"]);
    }
}
