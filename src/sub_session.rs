//! Conversation projection shared by IM channels and Application panels.
use remi_agentloop::prelude::{Message, ProtocolEvent, SubSessionEvent};

pub(crate) async fn append_event_log(
    data_dir: &std::path::Path,
    session_id: &str,
    event: &SubSessionEvent,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let directory = data_dir.join("tui-subsession-events");
    tokio::fs::create_dir_all(&directory).await?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(format!("{session_id}.jsonl")))
        .await?;
    let mut bytes = serde_json::to_vec(event)?;
    bytes.push(b'\n');
    file.write_all(&bytes).await?;
    Ok(())
}

pub(crate) async fn history_messages_for_run(
    data_dir: &std::path::Path,
    session_id: &str,
    event: &SubSessionEvent,
) -> Vec<Message> {
    if let ProtocolEvent::Custom { event_type, extra } = event.event.as_ref() {
        if event_type == "sub_session_done" {
            if let Some(output) = extra
                .get("final_output")
                .and_then(serde_json::Value::as_str)
            {
                let raw = tokio::fs::read_to_string(
                    data_dir
                        .join("tui-subsession-events")
                        .join(format!("{session_id}.jsonl")),
                )
                .await
                .unwrap_or_default();
                let streamed: String = raw
                    .lines()
                    .filter_map(|line| serde_json::from_str::<SubSessionEvent>(line).ok())
                    .filter(|previous| previous.sub_run_id == event.sub_run_id)
                    .filter_map(|previous| match *previous.event {
                        ProtocolEvent::Delta { content, .. } => Some(content),
                        _ => None,
                    })
                    .collect();
                if !streamed.is_empty() && streamed.trim() == output.trim() {
                    return vec![];
                }
            }
        }
    }
    history_messages(event)
}

pub(crate) fn run_input(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata?
        .get("input")?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}

pub(crate) fn history_messages(event: &SubSessionEvent) -> Vec<Message> {
    match event.event.as_ref() {
        ProtocolEvent::RunStart { metadata, .. } => vec![Message::user(
            run_input(metadata.as_ref())
                .or_else(|| event.title.clone())
                .unwrap_or_else(|| {
                    format!(
                        "Sub-session started: {} / {}",
                        event.agent_name, event.sub_thread_id.0
                    )
                }),
        )],
        ProtocolEvent::Delta { content, .. } if !content.is_empty() => {
            vec![Message::assistant(content.clone())]
        }
        ProtocolEvent::Custom { event_type, extra } if event_type == "sub_session_done" => extra
            .get("final_output")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(|s| vec![Message::assistant(s)])
            .unwrap_or_default(),
        ProtocolEvent::Error { message, .. } => {
            vec![Message::assistant(format!("Error: {message}"))]
        }
        ProtocolEvent::Cancelled => vec![Message::assistant("Error: cancelled")],
        _ => vec![],
    }
}

pub(crate) fn transcript(history: &[bot_core::ThreadHistoryMessage]) -> String {
    let mut text = String::new();
    let mut previous_role = "";
    for message in history {
        if message.text.is_empty() {
            continue;
        }
        if message.role != previous_role {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(match message.role.as_str() {
                "user" => "**User**\n",
                "assistant" => "**Assistant**\n",
                _ => "**Tool**\n",
            });
            previous_role = &message.role;
        }
        text.push_str(&message.text);
    }
    text
}

pub(crate) enum HistoryReplayItem {
    Message(bot_core::ThreadHistoryMessage),
    Event(SubSessionEvent),
}

/// Substitute event-rich turns for their mirrored history, retaining direct
/// continuation turns before, between and after them. Match the whole turn so
/// a repeated user prompt cannot consume a different conversation's reply.
pub(crate) fn merge_history_with_events(
    history: Vec<bot_core::ThreadHistoryMessage>,
    events: Vec<SubSessionEvent>,
) -> Vec<HistoryReplayItem> {
    let mut runs: Vec<Vec<SubSessionEvent>> = Vec::new();
    let mut run_indices = std::collections::HashMap::new();
    for event in events {
        let index = *run_indices
            .entry((event.sub_thread_id.clone(), event.sub_run_id.clone()))
            .or_insert_with(|| {
                runs.push(Vec::new());
                runs.len() - 1
            });
        runs[index].push(event);
    }
    fn push(parts: &mut Vec<(String, String)>, role: &str, text: &str) {
        if let Some((previous_role, previous_text)) = parts.last_mut() {
            if previous_role == role {
                previous_text.push_str(text);
                return;
            }
        }
        parts.push((role.to_string(), text.to_string()));
    }
    let mut result = Vec::new();
    let mut cursor = 0;
    for run in runs {
        let mut projected = Vec::new();
        let mut streamed = String::new();
        for event in &run {
            if let ProtocolEvent::Delta { content, .. } = event.event.as_ref() {
                streamed.push_str(content);
            }
            if let ProtocolEvent::Custom { event_type, extra } = event.event.as_ref() {
                if event_type == "sub_session_done"
                    && !streamed.is_empty()
                    && extra
                        .get("final_output")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| text.trim() == streamed.trim())
                {
                    continue;
                }
            }
            for message in history_messages(event) {
                let role = match message.role {
                    remi_agentloop::prelude::Role::User => "user",
                    remi_agentloop::prelude::Role::Assistant => "assistant",
                    remi_agentloop::prelude::Role::System => "system",
                    remi_agentloop::prelude::Role::Tool => "tool",
                };
                push(&mut projected, role, &message.content.text_content());
            }
        }
        let mut matched = None;
        for start in cursor..history.len() {
            if history[start].role != "user"
                || projected.first() != Some(&("user".into(), history[start].text.clone()))
            {
                continue;
            }
            let end = (start + 1..history.len())
                .find(|&index| history[index].role == "user")
                .unwrap_or(history.len());
            let mut actual = Vec::new();
            for message in &history[start..end] {
                push(&mut actual, &message.role, &message.text);
            }
            // History is fetched before the log. The active last turn may
            // have gained additional deltas while the log was being read.
            let active_prefix = end == history.len()
                && actual.len() <= projected.len()
                && actual.iter().enumerate().all(|(index, (role, text))| {
                    let (next_role, next_text) = &projected[index];
                    role == next_role
                        && if index + 1 == actual.len() && role == "assistant" {
                            next_text.starts_with(text)
                        } else {
                            next_text == text
                        }
                });
            if actual == projected || active_prefix {
                matched = Some((start, end));
                break;
            }
        }
        if let Some((start, end)) = matched {
            result.extend(
                history[cursor..start]
                    .iter()
                    .cloned()
                    .map(HistoryReplayItem::Message),
            );
            cursor = end;
        }
        result.extend(run.into_iter().map(HistoryReplayItem::Event));
    }
    result.extend(
        history[cursor..]
            .iter()
            .cloned()
            .map(HistoryReplayItem::Message),
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use remi_agentloop::prelude::{RunId, ThreadId};

    fn event(agent: &str, run: &str, protocol: ProtocolEvent) -> SubSessionEvent {
        SubSessionEvent::new(
            "tool",
            ThreadId("child".into()),
            RunId(run.into()),
            agent,
            Some("Display title".into()),
            1,
            protocol,
        )
    }

    fn history(role: &str, text: &str) -> bot_core::ThreadHistoryMessage {
        bot_core::ThreadHistoryMessage {
            id: uuid::Uuid::new_v4().to_string(),
            role: role.into(),
            text: text.into(),
            timestamp: None,
            tool_call_id: None,
            tool_calls: None,
            pretty: None,
        }
    }

    #[test]
    fn replay_merges_direct_continuations_and_repeated_prompts_without_duplicates() {
        let mut events = Vec::new();
        for (run, answer) in [("run-1", "first answer"), ("run-2", "second answer")] {
            events.push(event(
                "worker",
                run,
                ProtocolEvent::RunStart {
                    thread_id: "child".into(),
                    run_id: run.into(),
                    metadata: Some(serde_json::json!({"input":"same prompt"})),
                },
            ));
            events.push(event(
                "worker",
                run,
                ProtocolEvent::ToolCallStart {
                    id: run.into(),
                    name: "read".into(),
                },
            ));
            events.push(event(
                "worker",
                run,
                ProtocolEvent::Delta {
                    content: answer.into(),
                    role: None,
                },
            ));
            events.push(event(
                "worker",
                run,
                ProtocolEvent::Custom {
                    event_type: "sub_session_done".into(),
                    extra: serde_json::json!({"final_output":answer}),
                },
            ));
        }
        let history = vec![
            history("user", "before"),
            history("assistant", "before reply"),
            history("user", "same prompt"),
            history("assistant", "first answer"),
            history("user", "same prompt"),
            history("assistant", "direct reply"),
            history("user", "same prompt"),
            history("assistant", "second answer"),
            history("user", "continue"),
            history("assistant", "continued reply"),
        ];
        let merged = merge_history_with_events(history, events);
        let mut displayed = Vec::new();
        let mut tools = 0;
        for item in merged {
            match item {
                HistoryReplayItem::Message(message) => displayed.push(message.text),
                HistoryReplayItem::Event(event) => match event.event.as_ref() {
                    ProtocolEvent::RunStart { metadata, .. } => {
                        displayed.push(run_input(metadata.as_ref()).unwrap())
                    }
                    ProtocolEvent::Delta { content, .. } => displayed.push(content.clone()),
                    ProtocolEvent::ToolCallStart { .. } => tools += 1,
                    _ => (),
                },
            }
        }
        assert_eq!(tools, 2);
        assert_eq!(
            displayed,
            [
                "before",
                "before reply",
                "same prompt",
                "first answer",
                "same prompt",
                "direct reply",
                "same prompt",
                "second answer",
                "continue",
                "continued reply"
            ]
        );
    }

    #[tokio::test]
    async fn whitespace_chunks_survive_history_persistence_and_final_deduplication() {
        let directory = tempfile::tempdir().unwrap();
        let mut persisted = String::new();
        for text in ["hello", " ", "world", "\n", "next"] {
            let event = event(
                "worker",
                "run",
                ProtocolEvent::Delta {
                    content: text.into(),
                    role: None,
                },
            );
            for message in history_messages(&event) {
                persisted.push_str(&message.content.text_content());
            }
            append_event_log(directory.path(), "child", &event)
                .await
                .unwrap();
        }
        let done = event(
            "worker",
            "run",
            ProtocolEvent::Custom {
                event_type: "sub_session_done".into(),
                extra: serde_json::json!({"final_output":"hello world\nnext"}),
            },
        );
        assert!(history_messages_for_run(directory.path(), "child", &done)
            .await
            .is_empty());
        assert_eq!(persisted, "hello world\nnext");
    }

    #[test]
    fn replay_tolerates_a_log_that_advanced_after_the_history_snapshot() {
        let input = event(
            "worker",
            "run",
            ProtocolEvent::RunStart {
                thread_id: "child".into(),
                run_id: "run".into(),
                metadata: Some(serde_json::json!({"input":"task"})),
            },
        );
        let delta = event(
            "worker",
            "run",
            ProtocolEvent::Delta {
                content: "hello world".into(),
                role: None,
            },
        );
        for history in [
            vec![history("user", "task")],
            vec![history("user", "task"), history("assistant", "hello")],
        ] {
            let merged = merge_history_with_events(history, vec![input.clone(), delta.clone()]);
            assert_eq!(merged.len(), 2);
            assert!(merged
                .iter()
                .all(|item| matches!(item, HistoryReplayItem::Event(_))));
        }
    }

    #[test]
    fn child_output_uses_main_tool_and_final_events() {
        use crate::application::ApplicationEvent;
        use bot_core::CatEvent;
        let args = std::collections::HashMap::from([("call".into(), "{\"path\":\"test\"}".into())]);
        let result = output_events(
            &event(
                "worker",
                "run",
                ProtocolEvent::ToolResult {
                    id: "call".into(),
                    name: "read".into(),
                    result: "contents".into(),
                },
            ),
            &args,
        );
        assert!(
            matches!(&result[..], [ApplicationEvent::Cat(CatEvent::ToolCallResult { args, result, success: true, .. })]
            if args["path"] == "test" && result == "contents")
        );
        let done = output_events(
            &event(
                "acp",
                "run",
                ProtocolEvent::Custom {
                    event_type: "sub_session_done".into(),
                    extra: serde_json::json!({"final_output":"answer"}),
                },
            ),
            &args,
        );
        assert!(
            matches!(&done[..], [ApplicationEvent::ResponseCompleted { text, .. }, ApplicationEvent::Done] if text == "answer")
        );
    }

    #[tokio::test]
    async fn acp_and_agent_channels_keep_real_input_and_do_not_duplicate_streamed_final() {
        for agent in ["acp", "worker"] {
            let directory = tempfile::tempdir().unwrap();
            let input = event(
                agent,
                "run-1",
                ProtocolEvent::RunStart {
                    thread_id: "child".into(),
                    run_id: "run-1".into(),
                    metadata: Some(serde_json::json!({"input":"Actual task, not the title"})),
                },
            );
            let messages = history_messages(&input);
            assert_eq!(
                messages[0].content.text_content(),
                "Actual task, not the title"
            );
            append_event_log(directory.path(), "child", &input)
                .await
                .unwrap();
            for content in ["hello", " world"] {
                append_event_log(
                    directory.path(),
                    "child",
                    &event(
                        agent,
                        "run-1",
                        ProtocolEvent::Delta {
                            content: content.into(),
                            role: Some("assistant".into()),
                        },
                    ),
                )
                .await
                .unwrap();
            }
            let final_event = event(
                agent,
                "run-1",
                ProtocolEvent::Custom {
                    event_type: "sub_session_done".into(),
                    extra: serde_json::json!({"final_output":"hello world"}),
                },
            );
            assert!(
                history_messages_for_run(directory.path(), "child", &final_event)
                    .await
                    .is_empty()
            );
            let next_run = event(
                agent,
                "run-2",
                ProtocolEvent::Custom {
                    event_type: "sub_session_done".into(),
                    extra: serde_json::json!({"final_output":"hello world"}),
                },
            );
            assert_eq!(
                history_messages_for_run(directory.path(), "child", &next_run)
                    .await
                    .len(),
                1
            );
        }
    }
}

/// Adapt child protocol events to the same events consumed by the main chat.
/// All rendering, COT, tool cells and final delivery stay in its forwarder.
pub(crate) fn output_events(
    event: &SubSessionEvent,
    tool_args: &std::collections::HashMap<String, String>,
) -> Vec<crate::application::ApplicationEvent> {
    use crate::application::ApplicationEvent;
    use bot_core::CatEvent;
    let output = match event.event.as_ref() {
        ProtocolEvent::ToolDelta { id, name, delta } => CatEvent::ToolDelta {
            id: id.clone(),
            name: name.clone(),
            delta: delta.clone(),
        },
        ProtocolEvent::Delta { content, .. } => CatEvent::Text(content.clone()),
        ProtocolEvent::ThinkingEnd { content } if !content.is_empty() => {
            CatEvent::Thinking(content.clone())
        }
        ProtocolEvent::ToolCallStart { id, name } => CatEvent::ToolCallStart {
            id: id.clone(),
            name: name.clone(),
        },
        ProtocolEvent::ToolCallDelta {
            id,
            arguments_delta,
        } => CatEvent::ToolCallArgumentsDelta {
            id: id.clone(),
            delta: arguments_delta.clone(),
        },
        ProtocolEvent::ToolResult { id, name, result } => CatEvent::ToolCallResult {
            id: id.clone(),
            name: name.clone(),
            args: tool_args
                .get(id)
                .and_then(|args| serde_json::from_str(args).ok())
                .unwrap_or(serde_json::Value::Null),
            result: result.clone(),
            success: bot_core::tool_success(result),
            elapsed_ms: 0,
        },
        ProtocolEvent::Custom { event_type, extra } if event_type == "sub_session_done" => {
            let mut events = Vec::new();
            if let Some(text) = extra
                .get("final_output")
                .and_then(serde_json::Value::as_str)
                .filter(|text| !text.is_empty())
            {
                events.push(ApplicationEvent::ResponseCompleted {
                    message_id: event.sub_run_id.0.clone(),
                    text: text.to_string(),
                });
            }
            events.push(ApplicationEvent::Done);
            return events;
        }
        ProtocolEvent::Done => return vec![ApplicationEvent::Done],
        ProtocolEvent::Error { message, .. } => {
            CatEvent::Error(remi_agentloop::prelude::AgentError::other(message.clone()))
        }
        ProtocolEvent::Cancelled => CatEvent::Cancelled,
        _ => return Vec::new(),
    };
    vec![ApplicationEvent::Cat(output)]
}
