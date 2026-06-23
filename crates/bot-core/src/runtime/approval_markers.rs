use remi_agentloop::prelude::{
    RunId, SubSessionEvent, SubSessionEventPayload, ThreadId, ToolOutput,
};

use crate::{
    CatEvent, ToolApprovalDecision, ToolApprovalRequest, UserQuestionRequest, UserQuestionResponse,
};

use super::{
    SUBAGENT_APPROVAL_MARKER_AGENT, SUBAGENT_APPROVAL_REQUESTED_PREFIX,
    SUBAGENT_APPROVAL_RESOLVED_PREFIX, SUBAGENT_APPROVAL_UPDATED_PREFIX,
    USER_QUESTION_REQUESTED_PREFIX, USER_QUESTION_RESOLVED_PREFIX, USER_QUESTION_UPDATED_PREFIX,
};

pub(crate) fn tool_approval_requested_marker(
    sub_thread_id: &ThreadId,
    sub_run_id: &RunId,
    request: &ToolApprovalRequest,
) -> ToolOutput {
    subagent_approval_marker(
        sub_thread_id,
        sub_run_id,
        SUBAGENT_APPROVAL_REQUESTED_PREFIX,
        serde_json::to_string(request).unwrap_or_default(),
    )
}

pub(crate) fn tool_approval_updated_marker(
    sub_thread_id: &ThreadId,
    sub_run_id: &RunId,
    request: &ToolApprovalRequest,
) -> ToolOutput {
    subagent_approval_marker(
        sub_thread_id,
        sub_run_id,
        SUBAGENT_APPROVAL_UPDATED_PREFIX,
        serde_json::to_string(request).unwrap_or_default(),
    )
}

pub(crate) fn tool_approval_resolved_marker(
    sub_thread_id: &ThreadId,
    sub_run_id: &RunId,
    request: &ToolApprovalRequest,
    decision: ToolApprovalDecision,
) -> ToolOutput {
    subagent_approval_marker(
        sub_thread_id,
        sub_run_id,
        SUBAGENT_APPROVAL_RESOLVED_PREFIX,
        serde_json::to_string(&serde_json::json!({
            "request": request,
            "decision": decision,
        }))
        .unwrap_or_default(),
    )
}

pub(crate) fn user_question_requested_marker(request: &UserQuestionRequest) -> ToolOutput {
    user_question_marker(
        request,
        USER_QUESTION_REQUESTED_PREFIX,
        serde_json::to_string(request).unwrap_or_default(),
    )
}

pub(crate) fn user_question_updated_marker(request: &UserQuestionRequest) -> ToolOutput {
    user_question_marker(
        request,
        USER_QUESTION_UPDATED_PREFIX,
        serde_json::to_string(request).unwrap_or_default(),
    )
}

pub(crate) fn user_question_resolved_marker(
    request: &UserQuestionRequest,
    response: &UserQuestionResponse,
) -> ToolOutput {
    user_question_marker(
        request,
        USER_QUESTION_RESOLVED_PREFIX,
        serde_json::to_string(&serde_json::json!({
            "request": request,
            "response": response,
        }))
        .unwrap_or_default(),
    )
}

fn user_question_marker(
    request: &UserQuestionRequest,
    prefix: &str,
    payload: String,
) -> ToolOutput {
    subagent_approval_marker(
        &ThreadId(request.session_id.clone()),
        &RunId(request.run_id.clone()),
        prefix,
        payload,
    )
}

pub(crate) fn subagent_approval_marker(
    sub_thread_id: &ThreadId,
    sub_run_id: &RunId,
    prefix: &str,
    payload: String,
) -> ToolOutput {
    ToolOutput::SubSession(SubSessionEvent::new(
        String::new(),
        sub_thread_id.clone(),
        sub_run_id.clone(),
        SUBAGENT_APPROVAL_MARKER_AGENT,
        None,
        0,
        SubSessionEventPayload::Error {
            message: format!("{prefix}{payload}"),
        },
    ))
}

pub(crate) fn cat_event_from_subagent_approval_marker(event: &SubSessionEvent) -> Option<CatEvent> {
    if event.agent_name != SUBAGENT_APPROVAL_MARKER_AGENT {
        return None;
    }
    let SubSessionEventPayload::Error { message } = &event.payload else {
        return None;
    };
    if let Some(payload) = message.strip_prefix(SUBAGENT_APPROVAL_REQUESTED_PREFIX) {
        let request = serde_json::from_str(payload).ok()?;
        return Some(CatEvent::ToolApprovalRequested(request));
    }
    if let Some(payload) = message.strip_prefix(SUBAGENT_APPROVAL_UPDATED_PREFIX) {
        let request = serde_json::from_str(payload).ok()?;
        return Some(CatEvent::ToolApprovalUpdated(request));
    }
    if let Some(payload) = message.strip_prefix(SUBAGENT_APPROVAL_RESOLVED_PREFIX) {
        let value: serde_json::Value = serde_json::from_str(payload).ok()?;
        let request = serde_json::from_value(value.get("request")?.clone()).ok()?;
        let decision = serde_json::from_value(value.get("decision")?.clone()).ok()?;
        return Some(CatEvent::ToolApprovalResolved { request, decision });
    }
    if let Some(payload) = message.strip_prefix(USER_QUESTION_REQUESTED_PREFIX) {
        let request = serde_json::from_str(payload).ok()?;
        return Some(CatEvent::UserQuestionRequested(request));
    }
    if let Some(payload) = message.strip_prefix(USER_QUESTION_UPDATED_PREFIX) {
        let request = serde_json::from_str(payload).ok()?;
        return Some(CatEvent::UserQuestionUpdated(request));
    }
    if let Some(payload) = message.strip_prefix(USER_QUESTION_RESOLVED_PREFIX) {
        let value: serde_json::Value = serde_json::from_str(payload).ok()?;
        let request = serde_json::from_value(value.get("request")?.clone()).ok()?;
        let response = serde_json::from_value(value.get("response")?.clone()).ok()?;
        return Some(CatEvent::UserQuestionResolved { request, response });
    }
    None
}
