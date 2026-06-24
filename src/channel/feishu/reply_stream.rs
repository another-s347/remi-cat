use std::collections::HashMap;

use bot_core::{
    ToolApprovalDecision, ToolApprovalRequest, UserQuestionRequest, UserQuestionResponse,
    UserQuestionStatus,
};
use im_feishu::client::{
    build_tool_approval_card, build_tool_approval_resolved_card, build_user_question_card,
    build_user_question_resolved_card,
};
use im_feishu::{FeishuGateway, StreamingCard};
use tracing::warn;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FeishuReplyKind {
    Text,
    Thinking,
    ToolCall,
    ToolResult,
    Supervisor,
    Stats,
    Error,
}

impl FeishuReplyKind {
    fn is_standalone(self) -> bool {
        matches!(self, Self::Stats | Self::Error)
    }

    pub(crate) fn starts_new_message(self, active: Option<Self>) -> bool {
        match (self, active) {
            (Self::ToolResult, Some(Self::ToolCall)) => false,
            (Self::ToolCall, Some(Self::ToolCall)) => true,
            _ => active != Some(self) || self.is_standalone(),
        }
    }

    pub(crate) fn finishes_message(self) -> bool {
        matches!(self, Self::ToolResult | Self::Stats | Self::Error)
    }
}

pub(super) struct FeishuReplyStream {
    gateway: FeishuGateway,
    parent_message_id: String,
    active_kind: Option<FeishuReplyKind>,
    active_card: Option<StreamingCard>,
    tool_cards: HashMap<String, StreamingCard>,
    compaction_cards: HashMap<String, StreamingCard>,
    sub_session_cards: HashMap<String, StreamingCard>,
    approval_cards: HashMap<String, String>,
    question_cards: HashMap<String, String>,
}

impl FeishuReplyStream {
    pub(super) fn new(gateway: FeishuGateway, parent_message_id: String) -> Self {
        Self {
            gateway,
            parent_message_id,
            active_kind: None,
            active_card: None,
            tool_cards: HashMap::new(),
            compaction_cards: HashMap::new(),
            sub_session_cards: HashMap::new(),
            approval_cards: HashMap::new(),
            question_cards: HashMap::new(),
        }
    }

    pub(super) async fn push(&mut self, kind: FeishuReplyKind, chunk: &str) {
        if kind.starts_new_message(self.active_kind) {
            self.finish_active().await;
            self.active_card = Some(self.gateway.begin_streaming_reply(&self.parent_message_id));
            self.active_kind = Some(kind);
        }

        if let Some(card) = self.active_card.as_mut() {
            card.push(chunk).await.ok();
        }

        if kind.finishes_message() {
            self.finish_active().await;
        }
    }

    pub(super) async fn finish(&mut self) {
        self.finish_active().await;
        for (_, mut card) in self.tool_cards.drain() {
            card.finish().await.ok();
        }
        for (_, mut card) in self.compaction_cards.drain() {
            card.finish().await.ok();
        }
        for (_, mut card) in self.sub_session_cards.drain() {
            card.finish().await.ok();
        }
    }

    async fn finish_active(&mut self) {
        if let Some(mut card) = self.active_card.take() {
            card.finish().await.ok();
        }
        self.active_kind = None;
    }

    pub(super) async fn update_tool(&mut self, call_id: &str, line: &str, done: bool) {
        self.finish_active().await;
        let gateway = self.gateway.clone();
        let parent_message_id = self.parent_message_id.clone();
        let card = self
            .tool_cards
            .entry(call_id.to_string())
            .or_insert_with(|| gateway.begin_streaming_reply(&parent_message_id));
        if done {
            card.replace_final(line).await.ok();
            self.tool_cards.remove(call_id);
        } else {
            card.replace(line).await.ok();
        }
    }

    pub(super) async fn update_context_compaction(&mut self, id: &str, line: &str, done: bool) {
        self.finish_active().await;
        let gateway = self.gateway.clone();
        let parent_message_id = self.parent_message_id.clone();
        let card = self
            .compaction_cards
            .entry(id.to_string())
            .or_insert_with(|| gateway.begin_streaming_reply(&parent_message_id));
        if done {
            card.replace_final(line).await.ok();
            self.compaction_cards.remove(id);
        } else {
            card.replace(line).await.ok();
        }
    }

    pub(super) async fn update_sub_session(&mut self, id: &str, line: &str, done: bool) {
        self.finish_active().await;
        let gateway = self.gateway.clone();
        let parent_message_id = self.parent_message_id.clone();
        let card = self
            .sub_session_cards
            .entry(id.to_string())
            .or_insert_with(|| gateway.begin_streaming_reply(&parent_message_id));
        if done {
            card.replace_final(line).await.ok();
            self.sub_session_cards.remove(id);
        } else {
            card.replace(line).await.ok();
        }
    }

    pub(super) async fn approval_requested(&mut self, request: &ToolApprovalRequest) {
        self.finish_active().await;
        let card = build_tool_approval_card(
            &request.id,
            &request.tool_name,
            tool_risk_value(request.risk),
            &request.args_summary,
            approval_review_text(request).as_deref(),
        );
        match self
            .gateway
            .reply_card_raw(&self.parent_message_id, card)
            .await
        {
            Ok(card_message_id) => {
                self.approval_cards
                    .insert(request.id.clone(), card_message_id);
            }
            Err(err) => warn!("send approval card failed: {err:#}"),
        }
    }

    pub(super) async fn approval_updated(&mut self, request: &ToolApprovalRequest) {
        self.finish_active().await;
        let Some(card_message_id) = self.approval_cards.get(&request.id) else {
            return;
        };
        let card = build_tool_approval_card(
            &request.id,
            &request.tool_name,
            tool_risk_value(request.risk),
            &request.args_summary,
            approval_review_text(request).as_deref(),
        );
        if let Err(err) = self.gateway.update_card_raw(card_message_id, card).await {
            warn!("update approval card failed: {err:#}");
        }
    }

    pub(super) async fn approval_resolved(
        &mut self,
        request: &ToolApprovalRequest,
        decision: ToolApprovalDecision,
    ) {
        self.finish_active().await;
        let Some(card_message_id) = self.approval_cards.remove(&request.id) else {
            return;
        };
        let card = build_tool_approval_resolved_card(
            &request.tool_name,
            tool_risk_value(request.risk),
            &request.args_summary,
            tool_approval_decision_value(decision),
        );
        if let Err(err) = self.gateway.update_card_raw(&card_message_id, card).await {
            warn!("resolve approval card failed: {err:#}");
        }
    }

    pub(super) async fn user_question_requested(&mut self, request: &UserQuestionRequest) {
        self.finish_active().await;
        let options = request
            .options
            .iter()
            .map(|option| serde_json::to_value(option).unwrap_or(serde_json::Value::Null))
            .collect::<Vec<_>>();
        let card = build_user_question_card(
            &request.id,
            &request.question,
            request.reason.as_deref(),
            &options,
            request.allow_free_text,
            request.placeholder.as_deref(),
        );
        match self
            .gateway
            .reply_card_raw(&self.parent_message_id, card)
            .await
        {
            Ok(card_message_id) => {
                self.question_cards
                    .insert(request.id.clone(), card_message_id);
            }
            Err(err) => warn!("send user question card failed: {err:#}"),
        }
    }

    pub(super) async fn user_question_updated(&mut self, request: &UserQuestionRequest) {
        self.finish_active().await;
        let Some(card_message_id) = self.question_cards.get(&request.id) else {
            return;
        };
        let options = request
            .options
            .iter()
            .map(|option| serde_json::to_value(option).unwrap_or(serde_json::Value::Null))
            .collect::<Vec<_>>();
        let card = build_user_question_card(
            &request.id,
            &request.question,
            request.reason.as_deref(),
            &options,
            request.allow_free_text,
            request.placeholder.as_deref(),
        );
        if let Err(err) = self.gateway.update_card_raw(card_message_id, card).await {
            warn!("update user question card failed: {err:#}");
        }
    }

    pub(super) async fn user_question_resolved(
        &mut self,
        request: &UserQuestionRequest,
        response: &UserQuestionResponse,
    ) {
        self.finish_active().await;
        let Some(card_message_id) = self.question_cards.remove(&request.id) else {
            return;
        };
        let card = build_user_question_resolved_card(
            &request.question,
            user_question_status_value(response.status),
            response.answer_text.as_deref(),
        );
        if let Err(err) = self.gateway.update_card_raw(&card_message_id, card).await {
            warn!("resolve user question card failed: {err:#}");
        }
    }
}

fn tool_approval_decision_value(decision: ToolApprovalDecision) -> &'static str {
    match decision {
        ToolApprovalDecision::Deny => "deny",
        ToolApprovalDecision::AllowOnce => "allow_once",
        ToolApprovalDecision::AllowSession => "allow_session",
        ToolApprovalDecision::AllowSessionModelAuto => "allow_session_model_auto",
    }
}

fn user_question_status_value(status: UserQuestionStatus) -> &'static str {
    match status {
        UserQuestionStatus::Answered => "answered",
        UserQuestionStatus::Cancelled => "cancelled",
    }
}

fn tool_risk_value(risk: bot_core::approval::ToolRiskLevel) -> &'static str {
    match risk {
        bot_core::approval::ToolRiskLevel::Low => "low",
        bot_core::approval::ToolRiskLevel::Medium => "medium",
        bot_core::approval::ToolRiskLevel::High => "high",
    }
}

fn approval_review_text(request: &ToolApprovalRequest) -> Option<String> {
    let review = request.review.as_ref()?;
    let mut text = review.reason.trim().to_string();
    if !review.concerns.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str("Concerns:");
        for concern in &review.concerns {
            text.push_str("\n- ");
            text.push_str(concern);
        }
    }
    (!text.trim().is_empty()).then_some(text)
}
