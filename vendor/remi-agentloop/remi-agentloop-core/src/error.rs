use crate::types::{InterruptId, RunId, ThreadId};
use thiserror::Error;

// ── HttpTransportError ────────────────────────────────────────────────────────

/// Error returned by HTTP transport implementations.
///
/// Defined in `remi-core` so both core and transport crates can use it.
#[derive(Debug, Clone)]
pub struct HttpTransportError(pub String);

impl std::fmt::Display for HttpTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for HttpTransportError {}

impl HttpTransportError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

// ── AgentError ────────────────────────────────────────────────────────────────

/// All errors that can be produced by the agent loop and related infrastructure.
///
/// # Constructing errors
///
/// Prefer the constructor helpers over building enum variants directly:
///
/// ```ignore
/// AgentError::model("context length exceeded")
/// AgentError::tool("web_search", "DNS timeout")
/// AgentError::other("unexpected state")
/// ```
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("HTTP transport error: {0}")]
    HttpTransport(#[from] HttpTransportError),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("SSE parse error: {message}")]
    SseParse { message: String },

    #[error("Tool execution error [{tool_name}]: {message}")]
    ToolExecution { tool_name: String, message: String },

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Model error: {0}")]
    Model(String),

    #[error("Max turns ({max}) exceeded")]
    MaxTurnsExceeded { max: usize },

    #[error("Thread not found: {0}")]
    ThreadNotFound(ThreadId),

    #[error("Message not found: {0}")]
    MessageNotFound(crate::types::MessageId),

    #[error("Run not found: {0}")]
    RunNotFound(RunId),

    #[error("Interrupt not found: {0}")]
    InterruptNotFound(InterruptId),

    #[error("Resume incomplete: expected {expected} interrupt results, got {got}")]
    ResumeIncomplete { expected: usize, got: usize },

    #[error("Replay start index {requested} is out of bounds for thread {thread_id} with {available} messages")]
    ReplayIndexOutOfBounds {
        thread_id: ThreadId,
        requested: usize,
        available: usize,
    },

    #[error("Cannot replay from checkpoint status {status}")]
    ReplayFromCheckpointNotAllowed { status: String },

    #[error("Context store error: {0}")]
    Store(String),

    #[error("IO error: {0}")]
    Io(String),

    #[error("{0}")]
    Other(String),
}

impl Clone for AgentError {
    fn clone(&self) -> Self {
        match self {
            Self::HttpTransport(e) => Self::HttpTransport(e.clone()),
            Self::Json(e) => Self::Other(e.to_string()),
            Self::SseParse { message } => Self::SseParse {
                message: message.clone(),
            },
            Self::ToolExecution { tool_name, message } => Self::ToolExecution {
                tool_name: tool_name.clone(),
                message: message.clone(),
            },
            Self::ToolNotFound(s) => Self::ToolNotFound(s.clone()),
            Self::Model(s) => Self::Model(s.clone()),
            Self::MaxTurnsExceeded { max } => Self::MaxTurnsExceeded { max: *max },
            Self::ThreadNotFound(id) => Self::ThreadNotFound(id.clone()),
            Self::MessageNotFound(id) => Self::MessageNotFound(id.clone()),
            Self::RunNotFound(id) => Self::RunNotFound(id.clone()),
            Self::InterruptNotFound(id) => Self::InterruptNotFound(id.clone()),
            Self::ResumeIncomplete { expected, got } => Self::ResumeIncomplete {
                expected: *expected,
                got: *got,
            },
            Self::ReplayIndexOutOfBounds {
                thread_id,
                requested,
                available,
            } => Self::ReplayIndexOutOfBounds {
                thread_id: thread_id.clone(),
                requested: *requested,
                available: *available,
            },
            Self::ReplayFromCheckpointNotAllowed { status } => {
                Self::ReplayFromCheckpointNotAllowed {
                    status: status.clone(),
                }
            }
            Self::Store(s) => Self::Store(s.clone()),
            Self::Io(s) => Self::Io(s.clone()),
            Self::Other(s) => Self::Other(s.clone()),
        }
    }
}

impl AgentError {
    pub fn sse_parse(msg: impl Into<String>) -> Self {
        Self::SseParse {
            message: msg.into(),
        }
    }

    pub fn tool(tool_name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::ToolExecution {
            tool_name: tool_name.into(),
            message: message.into(),
        }
    }

    pub fn model(msg: impl Into<String>) -> Self {
        Self::Model(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
