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
    SupervisorThinking,
    SupervisorMessage,
    Supervisor,
    Stats,
    Error,
}

impl FeishuReplyKind {
    fn is_standalone(self) -> bool {
        matches!(self, Self::Stats | Self::Error)
    }

    pub(crate) fn starts_new_message(self, active: Option<Self>) -> bool {
        active != Some(self) || self.is_standalone()
    }

    pub(crate) fn finishes_message(self) -> bool {
        matches!(self, Self::ToolResult | Self::Stats | Self::Error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FeishuTurnLayout {
    /// A normal group message can become a topic. Keep every process cell as a
    /// separate card in that topic and leave only the final answer outside it.
    ThreadedProcessCells,
    /// Topic messages cannot create nested topics. Fold their process cells
    /// into one card in the current topic instead.
    CompressedProcessCard,
}

impl FeishuTurnLayout {
    pub(crate) fn for_message(
        chat_type: &str,
        chat_mode: Option<&str>,
        thread_id: Option<&str>,
    ) -> Self {
        if chat_type != "group" {
            return Self::CompressedProcessCard;
        }
        match chat_mode.map(str::trim) {
            // A real topic-mode group cannot contain a nested topic.
            Some("topic") => Self::CompressedProcessCard,
            // In a normal group, create the process topic even when the
            // inbound event happens to carry a thread id. Group mode, not the
            // message field, is the source of truth for this product layout.
            Some("group") => Self::ThreadedProcessCells,
            // Preserve the conservative behavior if group metadata cannot be
            // fetched (for example because the app lacks the chat-read scope).
            _ if thread_id.is_some_and(|value| !value.trim().is_empty()) => {
                Self::CompressedProcessCard
            }
            _ => Self::ThreadedProcessCells,
        }
    }
}

#[derive(Default)]
struct ProcessTimeline {
    cells: Vec<(Option<String>, FeishuReplyKind, String)>,
    active_stream: Option<(FeishuReplyKind, usize)>,
}

impl ProcessTimeline {
    fn append_stream(&mut self, kind: FeishuReplyKind, chunk: &str) {
        if let Some((active_kind, index)) = self.active_stream {
            if active_kind == kind {
                self.cells[index].2.push_str(chunk);
                return;
            }
        }
        let index = self.cells.len();
        self.cells.push((None, kind, chunk.to_string()));
        self.active_stream = Some((kind, index));
    }

    fn replace_stream(&mut self, kind: FeishuReplyKind, content: &str) {
        if let Some((active_kind, index)) = self.active_stream {
            if active_kind == kind {
                self.cells[index].2 = content.to_string();
                return;
            }
        }
        let index = self.cells.len();
        self.cells.push((None, kind, content.to_string()));
        self.active_stream = Some((kind, index));
    }

    fn push_fixed(&mut self, kind: FeishuReplyKind, content: String) {
        self.active_stream = None;
        self.cells.push((None, kind, content));
    }

    fn upsert(&mut self, key: String, kind: FeishuReplyKind, content: &str) -> bool {
        if let Some((_, _, body)) = self
            .cells
            .iter_mut()
            .find(|(cell_key, _, _)| cell_key.as_deref() == Some(key.as_str()))
        {
            *body = content.to_string();
            return false;
        }
        self.active_stream = None;
        self.cells.push((Some(key), kind, content.to_string()));
        true
    }

    fn break_stream(&mut self) {
        self.active_stream = None;
    }

    fn render(&self) -> String {
        self.cells
            .iter()
            .filter_map(|(_, _, body)| (!body.trim().is_empty()).then_some(body.trim()))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }
}

pub(super) struct FeishuReplyStream {
    gateway: FeishuGateway,
    parent_message_id: String,
    layout: FeishuTurnLayout,
    pending_final_text: String,
    final_output_committed: bool,
    process_topic_root_message_id: Option<String>,
    active_kind: Option<FeishuReplyKind>,
    active_card: Option<StreamingCard>,
    compressed_card: Option<StreamingCard>,
    process_timeline: ProcessTimeline,
    tool_cards: HashMap<String, StreamingCard>,
    compaction_cards: HashMap<String, StreamingCard>,
    sub_session_cards: HashMap<String, StreamingCard>,
    status_cards: HashMap<String, StreamingCard>,
    approval_cards: HashMap<String, String>,
    question_cards: HashMap<String, String>,
}

impl FeishuReplyStream {
    pub(super) fn new(
        gateway: FeishuGateway,
        parent_message_id: String,
        layout: FeishuTurnLayout,
    ) -> Self {
        Self {
            gateway,
            parent_message_id,
            layout,
            pending_final_text: String::new(),
            final_output_committed: false,
            process_topic_root_message_id: None,
            active_kind: None,
            active_card: None,
            compressed_card: None,
            process_timeline: ProcessTimeline::default(),
            tool_cards: HashMap::new(),
            compaction_cards: HashMap::new(),
            sub_session_cards: HashMap::new(),
            status_cards: HashMap::new(),
            approval_cards: HashMap::new(),
            question_cards: HashMap::new(),
        }
    }

    pub(super) async fn push(&mut self, kind: FeishuReplyKind, chunk: &str) {
        if kind == FeishuReplyKind::Text {
            self.finish_active().await;
            self.pending_final_text.push_str(chunk);
            return;
        }
        if kind == FeishuReplyKind::Error {
            self.flush_pending_as_process().await;
            self.finish_active().await;
            self.send_final(chunk).await;
            return;
        }
        self.flush_pending_as_process().await;
        if self.layout == FeishuTurnLayout::CompressedProcessCard {
            self.process_timeline.append_stream(kind, chunk);
            self.flush_compressed_process().await;
            if kind.finishes_message() {
                self.process_timeline.break_stream();
            }
            return;
        }
        if kind.starts_new_message(self.active_kind) {
            self.finish_active().await;
            self.active_card = Some(self.begin_process_card());
        }
        self.active_kind = Some(kind);

        if let Some(card) = self.active_card.as_mut() {
            if let Err(err) = card.push(chunk).await {
                warn!("stream process card failed: {err:#}");
            }
        }
        self.capture_process_topic_root_from_active();

        if kind.finishes_message() {
            self.finish_active().await;
        }
    }

    pub(super) async fn replace(&mut self, kind: FeishuReplyKind, content: &str) {
        self.flush_pending_as_process().await;
        if self.layout == FeishuTurnLayout::CompressedProcessCard {
            self.process_timeline.replace_stream(kind, content);
            self.flush_compressed_process().await;
            return;
        }
        if kind.starts_new_message(self.active_kind) {
            self.finish_active().await;
            self.active_card = Some(self.begin_process_card());
        }
        self.active_kind = Some(kind);
        if let Some(card) = self.active_card.as_mut() {
            if let Err(err) = card.replace(content).await {
                warn!("replace process card failed: {err:#}");
            }
        }
        self.capture_process_topic_root_from_active();
    }

    /// Emit an auxiliary/debug cell without changing which text cell the
    /// explicit final-output marker will commit.
    pub(super) async fn push_auxiliary(&mut self, kind: FeishuReplyKind, chunk: &str) {
        if self.layout == FeishuTurnLayout::CompressedProcessCard {
            self.process_timeline.append_stream(kind, chunk);
            self.flush_compressed_process().await;
            self.process_timeline.break_stream();
            return;
        }
        self.finish_active().await;
        let mut card = self.begin_process_card();
        card.replace_final(chunk).await.ok();
        self.capture_process_topic_root(card.message_id.clone());
    }

    pub(super) async fn start_new_cell(&mut self) {
        self.flush_pending_as_process().await;
        self.finish_active().await;
        self.process_timeline.break_stream();
    }

    pub(super) async fn finish(&mut self) {
        self.finish_active().await;
        if !self.pending_final_text.is_empty() {
            self.flush_pending_as_process().await;
        }
        if let Some(mut card) = self.compressed_card.take() {
            card.finish().await.ok();
        }
        for (_, mut card) in self.tool_cards.drain() {
            card.finish().await.ok();
        }
        for (_, mut card) in self.compaction_cards.drain() {
            card.finish().await.ok();
        }
        for (_, mut card) in self.sub_session_cards.drain() {
            card.finish().await.ok();
        }
        for (_, mut card) in self.status_cards.drain() {
            card.finish().await.ok();
        }
    }

    /// Commit the last text cell as the turn's final output. This is called
    /// only from the explicit run-completion marker, never from stream teardown.
    pub(super) async fn commit_final_output(&mut self) {
        if self.final_output_committed {
            return;
        }
        self.final_output_committed = true;
        self.finish_active().await;
        let final_text = std::mem::take(&mut self.pending_final_text);
        self.send_final(&final_text).await;
    }

    async fn finish_active(&mut self) {
        if let Some(mut card) = self.active_card.take() {
            card.finish().await.ok();
        }
        self.active_kind = None;
    }

    fn begin_process_card(&self) -> StreamingCard {
        let (parent_message_id, reply_in_thread) = process_reply_target(
            self.layout,
            &self.parent_message_id,
            self.process_topic_root_message_id.as_deref(),
        );
        if reply_in_thread {
            self.gateway.begin_streaming_thread_reply(parent_message_id)
        } else {
            self.gateway.begin_streaming_reply(parent_message_id)
        }
    }

    async fn send_process_raw(&mut self, card: serde_json::Value) -> anyhow::Result<String> {
        let (parent_message_id, reply_in_thread) = process_reply_target(
            self.layout,
            &self.parent_message_id,
            self.process_topic_root_message_id.as_deref(),
        );
        let parent_message_id = parent_message_id.to_string();
        let message_id = if reply_in_thread {
            self.gateway
                .reply_card_raw_in_thread(&parent_message_id, card)
                .await?
        } else {
            self.gateway
                .reply_card_raw(&parent_message_id, card)
                .await?
        };
        self.capture_process_topic_root(Some(message_id.clone()));
        Ok(message_id)
    }

    fn capture_process_topic_root(&mut self, message_id: Option<String>) {
        if self.layout == FeishuTurnLayout::ThreadedProcessCells
            && self.process_topic_root_message_id.is_none()
        {
            self.process_topic_root_message_id = message_id;
        }
    }

    fn capture_process_topic_root_from_active(&mut self) {
        let message_id = self
            .active_card
            .as_ref()
            .and_then(|card| card.message_id.clone());
        self.capture_process_topic_root(message_id);
    }

    async fn flush_pending_as_process(&mut self) {
        if self.pending_final_text.is_empty() {
            return;
        }
        let content = std::mem::take(&mut self.pending_final_text);
        if self.layout == FeishuTurnLayout::CompressedProcessCard {
            self.process_timeline
                .push_fixed(FeishuReplyKind::Text, content);
            self.flush_compressed_process().await;
        } else {
            let mut card = self.begin_process_card();
            card.replace_final(&content).await.ok();
            self.capture_process_topic_root(card.message_id.clone());
        }
    }

    async fn flush_compressed_process(&mut self) {
        let content = self.process_timeline.render();
        if content.is_empty() {
            return;
        }
        if self.compressed_card.is_none() {
            self.compressed_card = Some(self.begin_process_card());
        }
        if let Some(card) = self.compressed_card.as_mut() {
            card.replace(&content).await.ok();
        }
    }

    async fn send_final(&self, content: &str) {
        if content.trim().is_empty() {
            return;
        }
        let mut card = self.gateway.begin_streaming_reply(&self.parent_message_id);
        card.replace_final(content).await.ok();
    }

    pub(super) async fn update_tool(&mut self, call_id: &str, line: &str, done: bool) -> bool {
        let key = format!("tool:{call_id}");
        if self.layout == FeishuTurnLayout::CompressedProcessCard {
            let created = !self
                .process_timeline
                .cells
                .iter()
                .any(|(cell_key, _, _)| cell_key.as_deref() == Some(key.as_str()));
            if created {
                self.flush_pending_as_process().await;
            }
            self.process_timeline
                .upsert(key, FeishuReplyKind::ToolCall, line);
            self.flush_compressed_process().await;
            return created;
        }
        let created = !self.tool_cards.contains_key(call_id);
        if created {
            self.flush_pending_as_process().await;
            self.finish_active().await;
            let card = self.begin_process_card();
            self.tool_cards.insert(call_id.to_string(), card);
        }
        let message_id = {
            let card = self
                .tool_cards
                .get_mut(call_id)
                .expect("tool card inserted");
            if done {
                card.replace_final(line).await.ok();
            } else {
                card.replace(line).await.ok();
            }
            card.message_id.clone()
        };
        if done {
            self.tool_cards.remove(call_id);
        }
        self.capture_process_topic_root(message_id);
        created
    }

    pub(super) async fn update_context_compaction(
        &mut self,
        id: &str,
        line: &str,
        done: bool,
    ) -> bool {
        let key = format!("compaction:{id}");
        if self.layout == FeishuTurnLayout::CompressedProcessCard {
            let created = !self
                .process_timeline
                .cells
                .iter()
                .any(|(cell_key, _, _)| cell_key.as_deref() == Some(key.as_str()));
            self.process_timeline
                .upsert(key, FeishuReplyKind::ToolCall, line);
            self.flush_compressed_process().await;
            return created;
        }
        let created = !self.compaction_cards.contains_key(id);
        if created {
            self.finish_active().await;
            let card = self.begin_process_card();
            self.compaction_cards.insert(id.to_string(), card);
        }
        let message_id = {
            let card = self
                .compaction_cards
                .get_mut(id)
                .expect("compaction card inserted");
            if done {
                card.replace_final(line).await.ok();
            } else {
                card.replace(line).await.ok();
            }
            card.message_id.clone()
        };
        if done {
            self.compaction_cards.remove(id);
        }
        self.capture_process_topic_root(message_id);
        created
    }

    pub(super) async fn update_sub_session(&mut self, id: &str, line: &str, done: bool) -> bool {
        let key = format!("sub-session:{id}");
        if self.layout == FeishuTurnLayout::CompressedProcessCard {
            let created = !self
                .process_timeline
                .cells
                .iter()
                .any(|(cell_key, _, _)| cell_key.as_deref() == Some(key.as_str()));
            if created {
                self.flush_pending_as_process().await;
            }
            self.process_timeline
                .upsert(key, FeishuReplyKind::ToolCall, line);
            self.flush_compressed_process().await;
            return created;
        }
        let created = !self.sub_session_cards.contains_key(id);
        if created {
            self.flush_pending_as_process().await;
            self.finish_active().await;
            let card = self.begin_process_card();
            self.sub_session_cards.insert(id.to_string(), card);
        }
        let message_id = {
            let card = self
                .sub_session_cards
                .get_mut(id)
                .expect("sub-session card inserted");
            if done {
                card.replace_final(line).await.ok();
            } else {
                card.replace(line).await.ok();
            }
            card.message_id.clone()
        };
        if done {
            self.sub_session_cards.remove(id);
        }
        self.capture_process_topic_root(message_id);
        created
    }

    pub(super) async fn update_status(&mut self, id: &str, line: &str) -> bool {
        let key = format!("status:{id}");
        if self.layout == FeishuTurnLayout::CompressedProcessCard {
            let created = !self
                .process_timeline
                .cells
                .iter()
                .any(|(cell_key, _, _)| cell_key.as_deref() == Some(key.as_str()));
            self.process_timeline
                .upsert(key, FeishuReplyKind::ToolCall, line);
            self.flush_compressed_process().await;
            return created;
        }
        let created = !self.status_cards.contains_key(id);
        if created {
            self.finish_active().await;
            let card = self.begin_process_card();
            self.status_cards.insert(id.to_string(), card);
        }
        let card = self.status_cards.get_mut(id).expect("status card inserted");
        card.replace(line).await.ok();
        let message_id = card.message_id.clone();
        self.capture_process_topic_root(message_id);
        created
    }

    pub(super) async fn finish_status(&mut self, id: &str, line: &str) -> bool {
        if self.layout == FeishuTurnLayout::CompressedProcessCard {
            let key = format!("status:{id}");
            let exists = self
                .process_timeline
                .cells
                .iter()
                .any(|(cell_key, _, _)| cell_key.as_deref() == Some(key.as_str()));
            if exists {
                self.process_timeline
                    .upsert(key, FeishuReplyKind::ToolResult, line);
                self.flush_compressed_process().await;
            }
            return exists;
        }
        let Some(mut card) = self.status_cards.remove(id) else {
            return false;
        };
        card.replace_final(line).await.ok();
        true
    }

    pub(super) async fn approval_requested(&mut self, request: &ToolApprovalRequest) {
        self.flush_pending_as_process().await;
        self.finish_active().await;
        let card = build_tool_approval_card(
            &request.id,
            &request.tool_name,
            tool_risk_value(request.risk),
            &request.args_summary,
            approval_review_text(request).as_deref(),
        );
        let result = self.send_process_raw(card).await;
        match result {
            Ok(card_message_id) => {
                self.approval_cards
                    .insert(request.id.clone(), card_message_id);
            }
            Err(err) => warn!("send approval card failed: {err:#}"),
        }
    }

    pub(super) async fn approval_updated(&mut self, request: &ToolApprovalRequest) {
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
        self.flush_pending_as_process().await;
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
        let result = self.send_process_raw(card).await;
        match result {
            Ok(card_message_id) => {
                self.question_cards
                    .insert(request.id.clone(), card_message_id);
            }
            Err(err) => warn!("send user question card failed: {err:#}"),
        }
    }

    pub(super) async fn user_question_updated(&mut self, request: &UserQuestionRequest) {
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
        ToolApprovalDecision::AllowSameCommandSession => "allow_same_command_session",
        ToolApprovalDecision::AllowRiskLevelSession => "allow_risk_level_session",
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

fn process_reply_target<'a>(
    layout: FeishuTurnLayout,
    user_message_id: &'a str,
    process_root_message_id: Option<&'a str>,
) -> (&'a str, bool) {
    match (layout, process_root_message_id) {
        (FeishuTurnLayout::ThreadedProcessCells, Some(root_message_id)) => (root_message_id, true),
        _ => (user_message_id, false),
    }
}

#[cfg(test)]
mod tests {
    use super::{process_reply_target, FeishuReplyKind, FeishuTurnLayout, ProcessTimeline};

    #[test]
    fn normal_group_turns_process_cells_into_a_sub_topic() {
        assert_eq!(
            FeishuTurnLayout::for_message("group", Some("group"), None),
            FeishuTurnLayout::ThreadedProcessCells
        );
        assert_eq!(
            FeishuTurnLayout::for_message("group", Some("group"), Some("omt_existing")),
            FeishuTurnLayout::ThreadedProcessCells
        );
    }

    #[test]
    fn first_process_cell_becomes_root_and_later_cells_reply_in_its_topic() {
        assert_eq!(
            process_reply_target(FeishuTurnLayout::ThreadedProcessCells, "user-message", None,),
            ("user-message", false)
        );
        assert_eq!(
            process_reply_target(
                FeishuTurnLayout::ThreadedProcessCells,
                "user-message",
                Some("first-process-cell"),
            ),
            ("first-process-cell", true)
        );
        assert_eq!(
            process_reply_target(
                FeishuTurnLayout::CompressedProcessCard,
                "topic-user-message",
                Some("ignored-process-root"),
            ),
            ("topic-user-message", false)
        );
    }

    #[test]
    fn existing_topics_and_direct_messages_compress_process_cells() {
        assert_eq!(
            FeishuTurnLayout::for_message("group", Some("topic"), Some("omt_topic")),
            FeishuTurnLayout::CompressedProcessCard
        );
        assert_eq!(
            FeishuTurnLayout::for_message("p2p", Some("p2p"), None),
            FeishuTurnLayout::CompressedProcessCard
        );
    }

    #[test]
    fn compressed_process_card_preserves_tui_cell_boundaries() {
        let mut timeline = ProcessTimeline::default();
        timeline.append_stream(FeishuReplyKind::Thinking, "think one");
        timeline.append_stream(FeishuReplyKind::Thinking, " + two");
        assert!(timeline.upsert(
            "tool:1".to_string(),
            FeishuReplyKind::ToolCall,
            "tool running"
        ));
        assert!(!timeline.upsert(
            "tool:1".to_string(),
            FeishuReplyKind::ToolResult,
            "tool done"
        ));
        timeline.append_stream(FeishuReplyKind::Thinking, "think after tool");

        assert_eq!(timeline.cells.len(), 3);
        assert_eq!(
            timeline.render(),
            "think one + two\n\n---\n\ntool done\n\n---\n\nthink after tool"
        );
    }
}
