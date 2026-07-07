use remi_core::protocol::ProtocolEvent;

/// Encode a ProtocolEvent as an SSE text frame
pub fn encode_sse_event(event: &ProtocolEvent) -> String {
    let event_type = match event {
        ProtocolEvent::RunStart { .. } => "run_start",
        ProtocolEvent::Delta { .. } => "delta",
        ProtocolEvent::ThinkingStart => "thinking_start",
        ProtocolEvent::ThinkingEnd { .. } => "thinking_end",
        ProtocolEvent::ToolCallStart { .. } => "tool_call_start",
        ProtocolEvent::ToolCallDelta { .. } => "tool_call_delta",
        ProtocolEvent::ToolDelta { .. } => "tool_delta",
        ProtocolEvent::ToolResult { .. } => "tool_result",
        ProtocolEvent::SubSession { .. } => "sub_session",
        ProtocolEvent::Interrupt { .. } => "interrupt",
        ProtocolEvent::TurnStart { .. } => "turn_start",
        ProtocolEvent::Usage { .. } => "usage",
        ProtocolEvent::Error { .. } => "error",
        ProtocolEvent::Done => "done",
        ProtocolEvent::Cancelled => "cancelled",
        ProtocolEvent::NeedToolExecution { .. } => "need_tool_execution",
        ProtocolEvent::Custom { event_type, .. } => event_type.as_str(),
    };
    let data = serde_json::to_string(event).unwrap_or_default();
    format!("event: {event_type}\ndata: {data}\n\n")
}

/// Decode a single SSE data line into a ProtocolEvent
pub fn decode_sse_data(data: &str) -> Result<ProtocolEvent, remi_core::protocol::ProtocolError> {
    if data == "[DONE]" {
        return Ok(ProtocolEvent::Done);
    }
    serde_json::from_str(data).map_err(|e| remi_core::protocol::ProtocolError {
        code: "sse_parse_error".into(),
        message: e.to_string(),
    })
}
