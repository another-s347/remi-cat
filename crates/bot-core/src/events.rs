use remi_agentloop::prelude::{AgentError, Message};
use remi_agentloop::types::SubSessionEvent;

use crate::supervisor_workflow::{SupervisorTraceEvent, WorkflowReport};

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
