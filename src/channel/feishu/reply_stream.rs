use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use bot_core::{
    ToolApprovalDecision, ToolApprovalRequest, UserQuestionRequest, UserQuestionResponse,
    UserQuestionStatus,
};
use im_feishu::client::{
    build_tool_approval_card, build_tool_approval_resolved_card, build_user_question_card,
    build_user_question_resolved_card,
};
use im_feishu::{CotEvent, CotMessage, FeishuGateway};
use serde_json::json;
use tracing::warn;

const COT_FLUSH_INTERVAL: Duration = Duration::from_millis(350);
const COT_FLUSH_EVENT_LIMIT: usize = 12;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NarrativeKind {
    Text,
    Reasoning,
}

impl NarrativeKind {
    fn for_reply(kind: FeishuReplyKind) -> Self {
        match kind {
            FeishuReplyKind::Thinking | FeishuReplyKind::SupervisorThinking => Self::Reasoning,
            _ => Self::Text,
        }
    }
}

struct ActiveNarrative {
    id: String,
    kind: NarrativeKind,
    title: String,
    content: String,
}

/// Maps one assistant run to one native Feishu COT message. The final answer
/// deliberately remains an ordinary reply card so it is visible independently
/// from the collapsed process view.
pub(super) struct FeishuReplyStream {
    gateway: FeishuGateway,
    chat_id: String,
    parent_message_id: String,
    thread_id: String,
    run_id: String,
    pending_final_text: String,
    final_output_committed: bool,
    cot: Option<CotMessage>,
    cot_create_failed: bool,
    cot_completed: bool,
    pending_events: Vec<CotEvent>,
    last_cot_flush: Instant,
    active_narrative: Option<ActiveNarrative>,
    sequence: u64,
    last_event_timestamp: u64,
    activities: HashSet<String>,
    approval_cards: HashMap<String, String>,
    question_cards: HashMap<String, String>,
}

impl FeishuReplyStream {
    pub(super) fn new(gateway: FeishuGateway, chat_id: String, parent_message_id: String) -> Self {
        Self {
            gateway,
            chat_id: chat_id.clone(),
            thread_id: chat_id,
            parent_message_id,
            run_id: format!("run-{}", uuid::Uuid::new_v4()),
            pending_final_text: String::new(),
            final_output_committed: false,
            cot: None,
            cot_create_failed: false,
            cot_completed: false,
            pending_events: Vec::new(),
            last_cot_flush: Instant::now() - COT_FLUSH_INTERVAL,
            active_narrative: None,
            sequence: 0,
            last_event_timestamp: 0,
            activities: HashSet::new(),
            approval_cards: HashMap::new(),
            question_cards: HashMap::new(),
        }
    }

    pub(super) async fn push(&mut self, kind: FeishuReplyKind, chunk: &str) {
        if kind == FeishuReplyKind::Text {
            self.close_active_narrative().await;
            self.pending_final_text.push_str(chunk);
            return;
        }
        if kind == FeishuReplyKind::Error {
            self.flush_pending_as_process().await;
            self.fail_run("AGENT_ERROR", chunk).await;
            self.send_final(chunk).await;
            return;
        }
        self.flush_pending_as_process().await;
        self.append_narrative(kind, chunk).await;
    }

    pub(super) async fn replace(&mut self, kind: FeishuReplyKind, content: &str) {
        self.flush_pending_as_process().await;
        self.close_active_narrative().await;
        self.append_narrative(kind, content).await;
    }

    pub(super) async fn push_auxiliary(&mut self, kind: FeishuReplyKind, chunk: &str) {
        self.close_active_narrative().await;
        self.append_narrative(kind, chunk).await;
        self.close_active_narrative().await;
    }

    pub(super) async fn start_new_cell(&mut self) {
        self.flush_pending_as_process().await;
        self.close_active_narrative().await;
    }

    pub(super) async fn finish(&mut self) {
        if self.final_output_committed || self.cot_completed {
            return;
        }
        self.flush_pending_as_process().await;
        if self.cot.is_some() {
            self.fail_run("STREAM_ENDED", "运行未收到完成事件").await;
        }
    }

    pub(super) async fn commit_final_output(&mut self) {
        if self.final_output_committed {
            return;
        }
        self.final_output_committed = true;
        self.close_active_narrative().await;
        self.finish_run("done").await;
        let final_text = std::mem::take(&mut self.pending_final_text);
        self.send_final(&final_text).await;
    }

    pub(super) fn set_pending_final_text(&mut self, text: String) {
        if !self.final_output_committed {
            self.pending_final_text = text;
        }
    }

    pub(super) async fn interrupt_run(&mut self, reason: &str) {
        self.flush_pending_as_process().await;
        self.close_active_narrative().await;
        if self.cot.is_none() || self.cot_completed {
            return;
        }
        self.queue_event(
            "RUN_FINISHED",
            json!({
                "threadId": self.thread_id,
                "runId": self.run_id,
                "status": "interrupted",
                "reason": reason,
                "input": {"statusText": {"interrupted": {
                    "zh_cn": "任务已中断", "en_us": "Interrupted"
                }}}
            }),
        )
        .await;
        self.complete_run("error").await;
    }

    async fn ensure_cot(&mut self) -> bool {
        if self.cot.is_some() {
            return true;
        }
        if self.cot_create_failed || self.cot_completed {
            return false;
        }
        match self
            .gateway
            .create_cot(&self.chat_id, &self.parent_message_id)
            .await
        {
            Ok(cot) => {
                self.cot = Some(cot);
                let timestamp = self.next_event_timestamp();
                self.pending_events.push(CotEvent::at(
                    "RUN_STARTED",
                    json!({
                        "threadId": self.thread_id,
                        "runId": self.run_id,
                        "input": {"statusText": {
                            "running": {"zh_cn": "任务进行中", "en_us": "Working on it"},
                            "thinking": {"zh_cn": "正在思考", "en_us": "Thinking"},
                            "done": {"zh_cn": "任务已完成", "en_us": "Done"},
                            "error": {"zh_cn": "任务失败", "en_us": "Failed"},
                            "paused": {"zh_cn": "任务已暂停", "en_us": "Paused"},
                            "interrupted": {"zh_cn": "任务已中断", "en_us": "Interrupted"}
                        }}
                    }),
                    timestamp,
                ));
                true
            }
            Err(err) => {
                self.cot_create_failed = true;
                warn!("create Feishu COT failed: {err:#}");
                false
            }
        }
    }

    async fn queue_event(&mut self, event_type: &str, content: serde_json::Value) {
        if !self.enqueue_event(event_type, content).await {
            return;
        }
        self.flush_events_if_due().await;
    }

    async fn enqueue_event(&mut self, event_type: &str, content: serde_json::Value) -> bool {
        if !self.ensure_cot().await {
            return false;
        }
        let timestamp = self.next_event_timestamp();
        self.pending_events
            .push(CotEvent::at(event_type, content, timestamp));
        true
    }

    async fn flush_events_if_due(&mut self) {
        let force = self.pending_events.len() >= COT_FLUSH_EVENT_LIMIT
            || self.last_cot_flush.elapsed() >= COT_FLUSH_INTERVAL;
        if force {
            self.flush_events().await;
        }
    }

    fn next_event_timestamp(&mut self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let timestamp = now.max(self.last_event_timestamp.saturating_add(1));
        self.last_event_timestamp = timestamp;
        timestamp
    }

    async fn flush_events(&mut self) {
        if self.pending_events.is_empty() {
            return;
        }
        let Some(cot) = self.cot.clone() else {
            return;
        };
        let events = std::mem::take(&mut self.pending_events);
        match self.gateway.append_cot_events(&cot, &events).await {
            Ok(()) => self.last_cot_flush = Instant::now(),
            Err(err) => {
                warn!("append Feishu COT events failed: {err:#}");
                let mut retry = events;
                retry.append(&mut self.pending_events);
                self.pending_events = retry;
            }
        }
    }

    async fn append_narrative(&mut self, reply_kind: FeishuReplyKind, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let kind = NarrativeKind::for_reply(reply_kind);
        if self.active_narrative.as_ref().map(|active| active.kind) != Some(kind) {
            self.close_active_narrative().await;
            self.sequence += 1;
            self.active_narrative = Some(ActiveNarrative {
                id: format!("narrative-{}", self.sequence),
                kind,
                title: match reply_kind {
                    FeishuReplyKind::Thinking => "Thinking".to_string(),
                    FeishuReplyKind::SupervisorThinking => "Supervisor thinking".to_string(),
                    _ => process_title(delta),
                },
                content: String::new(),
            });
        }
        let Some(active) = self.active_narrative.as_mut() else {
            return;
        };
        active.content.push_str(delta);
    }

    async fn close_active_narrative(&mut self) {
        let Some(active) = self.active_narrative.take() else {
            return;
        };
        if active.content.is_empty() {
            return;
        }
        match active.kind {
            NarrativeKind::Text => {
                self.enqueue_event(
                    "TEXT_MESSAGE_START",
                    json!({"messageId": active.id, "role": "assistant"}),
                )
                .await;
                self.enqueue_event(
                    "TEXT_MESSAGE_CONTENT",
                    json!({"messageId": active.id, "delta": active.content}),
                )
                .await;
                self.enqueue_event("TEXT_MESSAGE_END", json!({"messageId": active.id}))
                    .await;
            }
            NarrativeKind::Reasoning => {
                self.enqueue_process_title(&active.id, &active.title).await;
                self.enqueue_event("REASONING_START", json!({"messageId": active.id}))
                    .await;
                self.enqueue_event(
                    "REASONING_MESSAGE_START",
                    json!({"messageId": active.id, "role": "reasoning"}),
                )
                .await;
                self.enqueue_event(
                    "REASONING_MESSAGE_CONTENT",
                    json!({"messageId": active.id, "delta": active.content}),
                )
                .await;
                self.enqueue_event("REASONING_MESSAGE_END", json!({"messageId": active.id}))
                    .await;
                self.enqueue_event("REASONING_END", json!({"messageId": active.id}))
                    .await;
            }
        }
        self.flush_events().await;
    }

    async fn enqueue_process_title(&mut self, cell_id: &str, title: &str) {
        if title.trim().is_empty() {
            return;
        }
        self.enqueue_event(
            "TEXT_MESSAGE_CHUNK",
            json!({
                "messageId": format!("title-{cell_id}"),
                "role": "assistant",
                "delta": title
            }),
        )
        .await;
    }

    async fn flush_pending_as_process(&mut self) {
        if self.pending_final_text.is_empty() {
            return;
        }
        let content = std::mem::take(&mut self.pending_final_text);
        self.close_active_narrative().await;
        self.append_narrative(FeishuReplyKind::SupervisorMessage, &content)
            .await;
        self.close_active_narrative().await;
    }

    async fn finish_run(&mut self, status: &str) {
        if self.cot.is_none() || self.cot_completed {
            return;
        }
        self.queue_event(
            "RUN_FINISHED",
            json!({
                "threadId": self.thread_id,
                "runId": self.run_id,
                "status": status,
                "input": {"statusText": {"done": {
                    "zh_cn": "任务已完成", "en_us": "Done"
                }}}
            }),
        )
        .await;
        self.complete_run("done").await;
    }

    async fn fail_run(&mut self, code: &str, message: &str) {
        self.close_active_narrative().await;
        if self.cot.is_none() || self.cot_completed {
            return;
        }
        self.queue_event(
            "RUN_ERROR",
            json!({
                "threadId": self.thread_id,
                "runId": self.run_id,
                "message": message,
                "code": code,
                "input": {"statusText": {"error": {
                    "zh_cn": "任务失败", "en_us": "Failed"
                }}}
            }),
        )
        .await;
        self.complete_run("error").await;
    }

    async fn complete_run(&mut self, reason: &str) {
        self.flush_events().await;
        let Some(cot) = self.cot.as_ref() else {
            return;
        };
        match self.gateway.complete_cot(cot, reason).await {
            Ok(()) => self.cot_completed = true,
            Err(err) => warn!("complete Feishu COT failed: {err:#}"),
        }
    }

    async fn send_final(&self, content: &str) {
        if content.trim().is_empty() {
            return;
        }
        let mut card = self.gateway.begin_streaming_reply(&self.parent_message_id);
        if let Err(error) = card.replace_final(content).await {
            warn!("send Feishu final reply failed: {error:#}");
            if content.contains("<at id=all></at>") {
                let fallback = content.replace("<at id=all></at>", "**[At 所有人失败]**");
                let mut fallback_card = self.gateway.begin_streaming_reply(&self.parent_message_id);
                if let Err(fallback_error) = fallback_card.replace_final(&fallback).await {
                    warn!("send Feishu broadcast fallback failed: {fallback_error:#}");
                }
            }
        }
    }

    pub(super) async fn update_tool(&mut self, call_id: &str, line: &str, done: bool) -> bool {
        self.update_activity(call_id, "tool", "bash", line, done)
            .await
    }

    pub(super) async fn update_context_compaction(
        &mut self,
        id: &str,
        line: &str,
        done: bool,
    ) -> bool {
        self.update_activity(
            &format!("compaction-{id}"),
            "context_compaction",
            "read",
            line,
            done,
        )
        .await
    }

    pub(super) async fn update_sub_session(&mut self, id: &str, line: &str, done: bool) -> bool {
        self.update_activity(
            &format!("sub-session-{id}"),
            "sub_session",
            "task",
            line,
            done,
        )
        .await
    }

    pub(super) async fn update_status(&mut self, id: &str, line: &str) -> bool {
        self.update_activity(&format!("status-{id}"), "status", "task", line, false)
            .await
    }

    pub(super) async fn finish_status(&mut self, id: &str, line: &str) -> bool {
        let key = format!("status-{id}");
        if !self.activities.contains(&key) {
            return false;
        }
        self.update_activity(&key, "status", "task", line, true)
            .await;
        true
    }

    async fn update_activity(
        &mut self,
        id: &str,
        name: &str,
        icon: &str,
        line: &str,
        done: bool,
    ) -> bool {
        self.flush_pending_as_process().await;
        self.close_active_narrative().await;
        let created = self.activities.insert(id.to_string());
        if created {
            self.enqueue_process_title(id, &process_title(line)).await;
            self.enqueue_event(
                "TOOL_CALL_START",
                json!({
                    "toolCallId": id,
                    "toolCallName": name,
                    "title": line,
                    "icon": icon,
                    "status": "running"
                }),
            )
            .await;
            self.enqueue_event("TOOL_CALL_END", json!({"toolCallId": id}))
                .await;
        }
        if done {
            self.enqueue_process_title(&format!("{id}-done"), &process_title(line))
                .await;
            self.enqueue_event(
                "TOOL_CALL_RESULT",
                json!({
                    "messageId": format!("result-{id}"),
                    "toolCallId": id,
                    "content": line,
                    "role": "tool",
                    "status": "completed"
                }),
            )
            .await;
        }
        if created || done {
            self.flush_events().await;
        }
        created
    }

    async fn send_interactive_card(&self, card: serde_json::Value) -> anyhow::Result<String> {
        self.gateway
            .reply_card_raw(&self.parent_message_id, card)
            .await
    }

    pub(super) async fn approval_requested(&mut self, request: &ToolApprovalRequest) {
        self.flush_pending_as_process().await;
        self.close_active_narrative().await;
        let card = build_tool_approval_card(
            &request.id,
            &request.tool_name,
            tool_risk_value(request.risk),
            &request.args_summary,
            approval_review_text(request).as_deref(),
        );
        match self.send_interactive_card(card).await {
            Ok(message_id) => {
                self.approval_cards.insert(request.id.clone(), message_id);
            }
            Err(err) => warn!("send approval card failed: {err:#}"),
        }
    }

    pub(super) async fn approval_updated(&mut self, request: &ToolApprovalRequest) {
        let Some(message_id) = self.approval_cards.get(&request.id) else {
            return;
        };
        let card = build_tool_approval_card(
            &request.id,
            &request.tool_name,
            tool_risk_value(request.risk),
            &request.args_summary,
            approval_review_text(request).as_deref(),
        );
        if let Err(err) = self.gateway.update_card_raw(message_id, card).await {
            warn!("update approval card failed: {err:#}");
        }
    }

    pub(super) async fn approval_resolved(
        &mut self,
        request: &ToolApprovalRequest,
        decision: ToolApprovalDecision,
    ) {
        let Some(message_id) = self.approval_cards.remove(&request.id) else {
            return;
        };
        let card = build_tool_approval_resolved_card(
            &request.tool_name,
            tool_risk_value(request.risk),
            &request.args_summary,
            tool_approval_decision_value(decision),
        );
        if let Err(err) = self.gateway.update_card_raw(&message_id, card).await {
            warn!("resolve approval card failed: {err:#}");
        }
    }

    pub(super) async fn user_question_requested(&mut self, request: &UserQuestionRequest) {
        self.flush_pending_as_process().await;
        self.close_active_narrative().await;
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
        match self.send_interactive_card(card).await {
            Ok(message_id) => {
                self.question_cards.insert(request.id.clone(), message_id);
            }
            Err(err) => warn!("send user question card failed: {err:#}"),
        }
    }

    pub(super) async fn user_question_updated(&mut self, request: &UserQuestionRequest) {
        let Some(message_id) = self.question_cards.get(&request.id) else {
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
        if let Err(err) = self.gateway.update_card_raw(message_id, card).await {
            warn!("update user question card failed: {err:#}");
        }
    }

    pub(super) async fn user_question_resolved(
        &mut self,
        request: &UserQuestionRequest,
        response: &UserQuestionResponse,
    ) {
        let Some(message_id) = self.question_cards.remove(&request.id) else {
            return;
        };
        let card = build_user_question_resolved_card(
            &request.question,
            user_question_status_value(response.status),
            response.answer_text.as_deref(),
        );
        if let Err(err) = self.gateway.update_card_raw(&message_id, card).await {
            warn!("resolve user question card failed: {err:#}");
        }
    }
}

fn process_title(content: &str) -> String {
    const MAX_CHARS: usize = 96;
    let title = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .trim_start_matches('#')
        .trim();
    let plain = title
        .chars()
        .filter(|character| !matches!(character, '*' | '`'))
        .collect::<String>();
    let mut chars = plain.trim().chars();
    let mut compact = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        compact.push('…');
    }
    compact
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

#[cfg(test)]
mod tests {
    use super::{process_title, FeishuReplyKind, NarrativeKind};

    #[test]
    fn reasoning_is_hidden_inside_expanded_cot() {
        assert_eq!(
            NarrativeKind::for_reply(FeishuReplyKind::Thinking),
            NarrativeKind::Reasoning
        );
        assert_eq!(
            NarrativeKind::for_reply(FeishuReplyKind::SupervisorThinking),
            NarrativeKind::Reasoning
        );
    }

    #[test]
    fn progress_copy_uses_cot_text_events() {
        assert_eq!(
            NarrativeKind::for_reply(FeishuReplyKind::SupervisorMessage),
            NarrativeKind::Text
        );
        assert_eq!(
            NarrativeKind::for_reply(FeishuReplyKind::Stats),
            NarrativeKind::Text
        );
    }

    #[test]
    fn process_copy_uses_the_first_non_empty_cell_title() {
        assert_eq!(
            process_title("\n**Running bash** — cargo test\nmore details"),
            "Running bash — cargo test"
        );
    }
}
