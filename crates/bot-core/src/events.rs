use remi_agentloop::prelude::{AgentError, Message};

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

// ── Top-level CatEvent ───────────────────────────────────────────────────────

/// All events emitted by `CatBot::stream()`.
pub enum CatEvent {
    /// Streaming text delta from the assistant.
    Text(String),
    /// Full message history + user_state captured at run completion.
    History(Vec<Message>, serde_json::Value),
    /// A skill tool mutated the skill store.
    Skill(SkillEvent),
    /// A todo tool mutated the todo list.
    Todo(TodoEvent),
    /// Run completed normally.
    Done,
    /// An error occurred (run aborted).
    Error(AgentError),
    /// Model thinking/reasoning content (from extended thinking).
    Thinking(String),
    /// A tool is being called (name + JSON arguments).
    ToolCall {
        name: String,
        args: serde_json::Value,
    },
    /// A tool returned its result.
    ToolCallResult { name: String, result: String },
    /// Token usage + elapsed time stats for the completed run.
    Stats {
        prompt_tokens: u32,
        completion_tokens: u32,
        elapsed_ms: u64,
    },
    /// Intermediate state snapshot — user_state after each tool round.
    /// Used by `CatBot` to persist user_state eagerly.
    StateUpdate(serde_json::Value),
}
