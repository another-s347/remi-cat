use crate::types::*;
use serde::{Deserialize, Serialize};

/// Standard protocol streaming response event — JSON-serialisable.
///
/// `ProtocolEvent` is the wire format used by [`ProtocolAgent`] over
/// HTTP SSE and WebSocket transports.  It is a lossless projection of
/// [`AgentEvent`](crate::types::AgentEvent) into a serialisable form.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ProtocolEvent {
    #[serde(rename = "run_start")]
    RunStart {
        thread_id: String,
        run_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<serde_json::Value>,
    },

    #[serde(rename = "delta")]
    Delta {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        role: Option<String>,
    },

    /// Chain-of-thought / reasoning delta from a thinking model (e.g. Kimi K2.5).
    #[serde(rename = "thinking_start")]
    ThinkingStart,

    /// Emitted when the thinking phase ends. Carries the full reasoning text.
    #[serde(rename = "thinking_end")]
    ThinkingEnd { content: String },

    #[serde(rename = "tool_call_start")]
    ToolCallStart { id: String, name: String },

    #[serde(rename = "tool_call_delta")]
    ToolCallDelta { id: String, arguments_delta: String },

    #[serde(rename = "tool_delta")]
    ToolDelta {
        id: String,
        name: String,
        delta: String,
    },

    #[serde(rename = "tool_result")]
    ToolResult {
        id: String,
        name: String,
        result: String,
    },

    #[serde(rename = "sub_session")]
    SubSession {
        parent_tool_call_id: String,
        sub_thread_id: String,
        sub_run_id: String,
        agent_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "crate::types::is_zero_u32")]
        depth: u32,
        event: SubSessionEventPayload,
    },

    #[serde(rename = "interrupt")]
    Interrupt { interrupts: Vec<InterruptInfo> },

    #[serde(rename = "turn_start")]
    TurnStart { turn: usize },

    #[serde(rename = "usage")]
    Usage {
        prompt_tokens: u32,
        completion_tokens: u32,
    },

    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },

    #[serde(rename = "done")]
    Done,

    #[serde(rename = "cancelled")]
    Cancelled,

    /// External tool execution needed — client should execute tools and resume.
    #[serde(rename = "need_tool_execution")]
    NeedToolExecution {
        state: crate::state::AgentState,
        tool_calls: Vec<ParsedToolCall>,
        completed_results: Vec<ToolCallOutcome>,
    },

    /// Arbitrary user-defined protocol event.  The `event_type` field carries
    /// the custom sub-type name; `extra` holds any additional JSON payload.
    #[serde(rename = "custom")]
    Custom {
        event_type: String,
        #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
        extra: serde_json::Value,
    },
}

// ── CustomProtocolEvent trait ────────────────────────────────────────────────

/// Trait for types that can be losslessly round-tripped through
/// [`ProtocolEvent::Custom`].  Implement this on any struct / enum that you
/// want to embed in the standard event stream.
///
/// ```rust,ignore
/// #[derive(serde::Serialize, serde::Deserialize)]
/// struct MyEvent { value: u32 }
///
/// impl CustomProtocolEvent for MyEvent {
///     const EVENT_TYPE: &'static str = "my_event";
/// }
///
/// let ev = MyEvent { value: 42 }.to_protocol_event().unwrap();
/// let back: MyEvent = MyEvent::from_protocol_event(&ev).unwrap().unwrap();
/// ```
pub trait CustomProtocolEvent: Sized + serde::Serialize + serde::de::DeserializeOwned {
    /// Unique string tag that identifies this custom event type.
    const EVENT_TYPE: &'static str;

    /// Wrap `self` into a [`ProtocolEvent::Custom`].
    fn to_protocol_event(&self) -> Result<ProtocolEvent, serde_json::Error> {
        Ok(ProtocolEvent::Custom {
            event_type: Self::EVENT_TYPE.to_owned(),
            extra: serde_json::to_value(self)?,
        })
    }

    /// Try to extract `Self` from a [`ProtocolEvent`].  Returns `None` when
    /// the event is not a `Custom` event or the `event_type` tag does not
    /// match; returns `Some(Err(_))` if deserialization fails.
    fn from_protocol_event(event: &ProtocolEvent) -> Option<Result<Self, serde_json::Error>> {
        if let ProtocolEvent::Custom { event_type, extra } = event {
            if event_type == Self::EVENT_TYPE {
                return Some(serde_json::from_value(extra.clone()));
            }
        }
        None
    }
}

/// Convert any [`CustomProtocolEvent`] into a [`ProtocolEvent`] via the
/// `Custom` variant.  Panics if serialization fails (use
/// [`CustomProtocolEvent::to_protocol_event`] for a fallible version).
impl<T: CustomProtocolEvent> From<T> for ProtocolEvent {
    fn from(value: T) -> Self {
        value
            .to_protocol_event()
            .expect("CustomProtocolEvent serialization failed")
    }
}

/// Error type for protocol-level failures.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

impl From<crate::error::AgentError> for ProtocolError {
    fn from(e: crate::error::AgentError) -> Self {
        ProtocolError {
            code: "agent_error".into(),
            message: e.to_string(),
        }
    }
}

/// Marker trait for agents that speak the standard protocol.
///
/// Any [`Agent`](crate::agent::Agent) with `Request = LoopInput`,
/// `Response = ProtocolEvent`, and `Error = ProtocolError` is automatically
/// a `ProtocolAgent` via a blanket impl.
///
/// Uses [`LoopInput`](crate::types::LoopInput) as Request (serialisable,
/// supports both start and resume), [`ProtocolEvent`] as Response.
pub trait ProtocolAgent:
    crate::agent::Agent<Request = LoopInput, Response = ProtocolEvent, Error = ProtocolError>
{
}

impl<T> ProtocolAgent for T where
    T: crate::agent::Agent<Request = LoopInput, Response = ProtocolEvent, Error = ProtocolError>
{
}

// ── Conversions ───────────────────────────────────────────────────────────────

impl From<AgentEvent> for ProtocolEvent {
    fn from(e: AgentEvent) -> Self {
        match e {
            AgentEvent::RunStart {
                thread_id,
                run_id,
                metadata,
            } => ProtocolEvent::RunStart {
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
                metadata,
            },
            AgentEvent::TextDelta(s) => ProtocolEvent::Delta {
                content: s,
                role: None,
            },
            AgentEvent::ThinkingStart => ProtocolEvent::ThinkingStart,
            AgentEvent::ThinkingEnd { content } => ProtocolEvent::ThinkingEnd { content },
            AgentEvent::ToolCallStart { id, name } => ProtocolEvent::ToolCallStart { id, name },
            AgentEvent::ToolCallArgumentsDelta { id, delta } => ProtocolEvent::ToolCallDelta {
                id,
                arguments_delta: delta,
            },
            AgentEvent::ToolDelta { id, name, delta } => {
                ProtocolEvent::ToolDelta { id, name, delta }
            }
            AgentEvent::ToolResult { id, name, result } => {
                ProtocolEvent::ToolResult { id, name, result }
            }
            AgentEvent::SubSession(event) => ProtocolEvent::SubSession {
                parent_tool_call_id: event.parent_tool_call_id,
                sub_thread_id: event.sub_thread_id.to_string(),
                sub_run_id: event.sub_run_id.to_string(),
                agent_name: event.agent_name,
                title: event.title,
                depth: event.depth,
                event: event.payload,
            },
            AgentEvent::Interrupt { interrupts } => ProtocolEvent::Interrupt { interrupts },
            AgentEvent::TurnStart { turn } => ProtocolEvent::TurnStart { turn },
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
            } => ProtocolEvent::Usage {
                prompt_tokens,
                completion_tokens,
            },
            AgentEvent::Done => ProtocolEvent::Done,
            AgentEvent::Cancelled => ProtocolEvent::Cancelled,
            AgentEvent::Error(e) => ProtocolEvent::Error {
                message: e.to_string(),
                code: None,
            },
            AgentEvent::Checkpoint(_) => ProtocolEvent::Done, // internal, filtered out
            AgentEvent::NeedToolExecution {
                state,
                tool_calls,
                completed_results,
            } => ProtocolEvent::NeedToolExecution {
                state,
                tool_calls,
                completed_results,
            },
        }
    }
}
