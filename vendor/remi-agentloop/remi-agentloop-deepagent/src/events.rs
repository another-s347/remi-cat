//! Deep-agent event types.
//!
//! [`DeepAgentEvent`] wraps the inner [`AgentEvent`] and adds higher-level
//! events emitted by the todo and skill layers.

use remi_core::types::{AgentEvent, Message};

// ── TodoEvent ─────────────────────────────────────────────────────────────────

/// Events emitted when the todo list is mutated.
#[derive(Debug, Clone)]
pub enum TodoEvent {
    Added { id: u64, content: String },
    Updated { id: u64, content: String },
    Completed { id: u64 },
    Removed { id: u64 },
}

// ── SkillEvent ────────────────────────────────────────────────────────────────

/// Events emitted when the skill store changes.
#[derive(Debug, Clone)]
pub enum SkillEvent {
    Saved { name: String, path: String },
    Deleted { name: String },
}

// ── DeepAgentEvent ────────────────────────────────────────────────────────────

/// The top-level event enum exposed by [`DeepAgent`](crate::agent::DeepAgent).
///
/// Consumers can match on `Agent(AgentEvent::TextDelta(...))` for streaming
/// text, `Todo(...)` for task-list updates, and `Skill(...)` for skill changes.
#[derive(Debug, Clone)]
pub enum DeepAgentEvent {
    Agent(AgentEvent),
    Todo(TodoEvent),
    Skill(SkillEvent),
    /// Full conversation history emitted just before `Agent(Done)`.
    /// Pass this as `history` on the next `chat_with_history()` call to
    /// maintain multi-turn context across runs.
    History(Vec<Message>),
}

impl From<AgentEvent> for DeepAgentEvent {
    fn from(e: AgentEvent) -> Self {
        DeepAgentEvent::Agent(e)
    }
}
