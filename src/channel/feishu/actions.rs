use std::rc::Rc;

use anyhow::Context;
use bot_core::{ToolApprovalDecision, UserQuestionResponse, UserQuestionStatus};
use im_feishu::FeishuGateway;
use tracing::warn;

use crate::core::Runtime;

pub(super) async fn process_feishu_card_action(
    runtime: Rc<Runtime>,
    gateway: FeishuGateway,
    card_message_id: String,
    action_value: serde_json::Value,
    user_open_id: String,
) -> anyhow::Result<()> {
    let action = action_value
        .get("action")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if action == "user_question_answer" || action == "user_question_cancel" {
        let question_id = action_value
            .get("question_id")
            .and_then(|value| value.as_str())
            .context("missing question_id in user question action")?;
        let selected_option_ids = action_value
            .get("selected_option_ids")
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let free_text = action_value
            .get("free_text")
            .or_else(|| action_value.pointer("/form_value/free_text"))
            .or_else(|| action_value.pointer("/form_values/free_text"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let status = if action == "user_question_cancel" {
            UserQuestionStatus::Cancelled
        } else {
            UserQuestionStatus::Answered
        };
        let answer_text =
            build_user_question_answer_text(&selected_option_ids, free_text.as_deref(), status);
        let response = UserQuestionResponse {
            question_id: question_id.to_string(),
            status,
            selected_option_ids,
            free_text,
            answer_text: Some(answer_text),
            answered_at: None,
            source: Some(format!("feishu:{user_open_id}")),
        };
        let resolved = runtime
            .bot
            .user_question_manager()
            .answer(question_id, response)
            .await;
        if resolved.is_none() {
            warn!(
                question_id,
                user = %user_open_id,
                "user question card action did not match a pending request"
            );
            gateway
                .update_card_raw(
                    &card_message_id,
                    build_tool_approval_notice_card("Question is no longer pending."),
                )
                .await
                .ok();
        }
        return Ok(());
    }
    if action != "approval_decide" {
        return Ok(());
    }
    let approval_id = action_value
        .get("approval_id")
        .and_then(|value| value.as_str())
        .context("missing approval_id in approval action")?;
    let decision = action_value
        .get("decision")
        .and_then(|value| value.as_str())
        .and_then(parse_tool_approval_decision)
        .context("invalid approval decision")?;
    let resolved = runtime
        .bot
        .approval_manager()
        .decide(approval_id, decision)
        .await;
    if resolved.is_none() {
        warn!(
            approval_id,
            user = %user_open_id,
            "approval card action did not match a pending request"
        );
        gateway
            .update_card_raw(
                &card_message_id,
                build_tool_approval_notice_card("Approval is no longer pending."),
            )
            .await
            .ok();
    }
    Ok(())
}

fn parse_tool_approval_decision(value: &str) -> Option<ToolApprovalDecision> {
    match value {
        "deny" => Some(ToolApprovalDecision::Deny),
        "allow_once" => Some(ToolApprovalDecision::AllowOnce),
        "allow_session" => Some(ToolApprovalDecision::AllowSession),
        "allow_session_model_auto" => Some(ToolApprovalDecision::AllowSessionModelAuto),
        _ => None,
    }
}

fn build_user_question_answer_text(
    selected_option_ids: &[String],
    free_text: Option<&str>,
    status: UserQuestionStatus,
) -> String {
    if status == UserQuestionStatus::Cancelled {
        return "User cancelled the question.".to_string();
    }
    let mut parts = Vec::new();
    if !selected_option_ids.is_empty() {
        parts.push(format!(
            "Selected option ids: {}",
            selected_option_ids.join(", ")
        ));
    }
    if let Some(text) = free_text {
        parts.push(format!("Free-text answer: {text}"));
    }
    if parts.is_empty() {
        "User answered without additional text.".to_string()
    } else {
        parts.join("\n")
    }
}

fn build_tool_approval_notice_card(message: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "body": {
            "elements": [{
                "tag": "markdown",
                "content": message
            }]
        },
        "header": {
            "title": { "tag": "plain_text", "content": "Tool approval" },
            "template": "grey"
        }
    })
}
