use remi_agentloop::prelude::{Content, Message, MessageId, Role};

use crate::{GoalMaxRounds, WorkflowMaxRounds, WorkflowReport};

pub(super) enum WorkflowRoundOutcome {
    NoWorkflow,
    Report(WorkflowReport),
    Continue {
        report: WorkflowReport,
        message: String,
    },
}

pub(super) fn workflow_round_allows_continue(
    max_rounds: &WorkflowMaxRounds,
    completed_continuations: u32,
) -> bool {
    match max_rounds {
        GoalMaxRounds::Limited(max) => completed_continuations < *max,
        GoalMaxRounds::Unlimited => true,
    }
}

pub(super) fn hook_context_message(event: &str, content: String) -> Message {
    Message {
        id: MessageId::new(),
        role: Role::System,
        content: Content::text(format!(
            "Hook {event} provided additional context:\n{content}"
        )),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        reasoning_content: None,
        metadata: None,
    }
}
