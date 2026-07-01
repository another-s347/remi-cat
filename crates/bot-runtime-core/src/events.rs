use remi_agentloop::prelude::{AgentError, AgentState, Message};
use remi_agentloop::types::SubSessionEvent;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CoreModelInputTotals {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum CoreAgentEvent {
    Text(String),
    Thinking(String),
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
    ToolCallResult {
        id: String,
        name: String,
        args: serde_json::Value,
        result: String,
        success: bool,
        elapsed_ms: u64,
    },
    SubSession(SubSessionEvent),
    Custom {
        event_type: String,
        extra: serde_json::Value,
    },
    History(Vec<Message>, serde_json::Value),
    Checkpoint(AgentState),
    Stats {
        prompt_tokens: u32,
        completion_tokens: u32,
        elapsed_ms: u64,
    },
    StateUpdate(serde_json::Value),
    Done {
        state: Option<AgentState>,
    },
    Cancelled,
    Error(AgentError),
}
