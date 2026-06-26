use remi_agentloop::types::{SubSessionEvent, SubSessionEventPayload};

use super::FeishuReplyKind;

pub(crate) fn format_context_compaction_line(event: &bot_core::ContextCompactionEvent) -> String {
    match event.status {
        bot_core::ContextCompactionStatus::Started => format!(
            "🧠 正在压缩上下文：压缩 {} 条，保留 {} 条",
            event.compacted_messages, event.remaining_messages
        ),
        bot_core::ContextCompactionStatus::Completed => format!(
            "🧠 上下文已压缩：压缩 {} 条，保留 {} 条",
            event.compacted_messages, event.remaining_messages
        ),
        bot_core::ContextCompactionStatus::Failed => format!(
            "🧠 上下文压缩失败：{}",
            event.error.as_deref().unwrap_or("unknown error")
        ),
    }
}

pub(super) fn format_feishu_sub_session_line(event: &SubSessionEvent) -> Option<String> {
    let (status, detail) = match &event.payload {
        SubSessionEventPayload::Start => ("running", "started".to_string()),
        SubSessionEventPayload::Delta { .. } => return None,
        SubSessionEventPayload::ThinkingStart => ("running", "thinking".to_string()),
        SubSessionEventPayload::ThinkingEnd { .. } => ("running", "thinking complete".to_string()),
        SubSessionEventPayload::TurnStart { turn } => ("running", format!("turn {turn}")),
        SubSessionEventPayload::ToolCallStart { name, .. } => {
            ("running", format!("calling tool `{name}`"))
        }
        SubSessionEventPayload::ToolCallArgumentsDelta { id, delta } => (
            "running",
            format!("tool arguments `{id}`: {}", single_line(delta)),
        ),
        SubSessionEventPayload::ToolDelta { name, delta, .. } => (
            "running",
            format!("tool `{name}` output: {}", single_line(delta)),
        ),
        SubSessionEventPayload::ToolResult { name, result, .. } => (
            "running",
            format!("tool `{name}` result: {}", single_line(result)),
        ),
        SubSessionEventPayload::Done { .. } => ("done", "done".to_string()),
        SubSessionEventPayload::Error { message } => ("error", single_line(message)),
    };
    Some(truncate_tool_line(&format!(
        "**Sub-session** `{}` / `{}` · {status} · {detail}",
        event.agent_name, event.sub_thread_id.0
    )))
}

pub(crate) fn format_feishu_tool_line(pretty: &bot_core::PrettyToolCall) -> String {
    let (icon, status) = match pretty.status {
        bot_core::PrettyToolStatus::Running => ("⏳", "运行中"),
        bot_core::PrettyToolStatus::Success => ("✅", "成功"),
        bot_core::PrettyToolStatus::Error => ("❌", "失败"),
    };
    let elapsed = pretty
        .elapsed_ms
        .map(format_elapsed)
        .map(|value| format!(" · {value}"))
        .unwrap_or_default();
    truncate_tool_line(&format!(
        "{icon} {status} **{}** — {}{elapsed}",
        single_line(&pretty.title),
        single_line(&pretty.summary)
    ))
}

fn truncate_tool_line(line: &str) -> String {
    const MAX_CHARS: usize = 140;
    if line.chars().count() <= MAX_CHARS {
        return line.to_string();
    }
    let mut truncated = line.chars().take(MAX_CHARS).collect::<String>();
    truncated.push('…');
    truncated
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_elapsed(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        let minutes = ms / 60_000;
        let seconds = (ms % 60_000) / 1_000;
        format!("{minutes}m{seconds:02}s")
    } else if ms < 86_400_000 {
        let hours = ms / 3_600_000;
        let minutes = (ms % 3_600_000) / 60_000;
        format!("{hours}h{minutes:02}m")
    } else {
        let days = ms / 86_400_000;
        let hours = (ms % 86_400_000) / 3_600_000;
        format!("{days}d{hours:02}h")
    }
}

pub(super) fn format_supervisor_progress(event: &bot_core::SupervisorTraceEvent) -> String {
    match event {
        bot_core::SupervisorTraceEvent::Thinking { content } => {
            format!("\n**Thinking**\n{}\n", fenced_block("text", content))
        }
        bot_core::SupervisorTraceEvent::ToolCall { name, args } => {
            let json = serde_json::to_string_pretty(args).unwrap_or_default();
            format!("\n**Tool `{name}`**\n{}\n", fenced_block("json", &json))
        }
        bot_core::SupervisorTraceEvent::ToolResult { name, result } => format!(
            "\n**Tool result `{name}`**\n{}\n",
            fenced_block("text", result)
        ),
        bot_core::SupervisorTraceEvent::OutputDelta { content } => content.clone(),
        bot_core::SupervisorTraceEvent::Output { content } => {
            format!("\n**Output**\n{}\n", fenced_block("json", content))
        }
        bot_core::SupervisorTraceEvent::AgentMessage { content } => {
            format!("\n**Agent message**\n{}\n", fenced_block("text", content))
        }
    }
}

pub(super) fn supervisor_reply_kind(event: &bot_core::SupervisorTraceEvent) -> FeishuReplyKind {
    match event {
        bot_core::SupervisorTraceEvent::Thinking { .. } => FeishuReplyKind::Thinking,
        bot_core::SupervisorTraceEvent::ToolCall { .. } => FeishuReplyKind::ToolCall,
        bot_core::SupervisorTraceEvent::ToolResult { .. } => FeishuReplyKind::ToolResult,
        bot_core::SupervisorTraceEvent::OutputDelta { .. }
        | bot_core::SupervisorTraceEvent::Output { .. }
        | bot_core::SupervisorTraceEvent::AgentMessage { .. } => FeishuReplyKind::Supervisor,
    }
}

pub(super) fn fenced_block(lang: &str, content: &str) -> String {
    let sanitized = content.replace("```", "'''");
    format!("```{lang}\n{}\n```", sanitized.trim())
}
