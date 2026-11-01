pub mod backend;
pub mod tools;

pub use backend::HybridTodoBackend;
pub use tools::{
    current_todo_card_markdown, latest_unfinished_batch_system_prompt, todos_from_user_state,
    TodoAddTool, TodoCompleteTool, TodoItem, TodoListTool, TodoRemoveTool, TodoTool,
    TodoUpdateTool,
};

use remi_agentloop::prelude::ParsedToolCall;
use serde::Deserialize;

use crate::events::TodoEvent;

pub(crate) const LEGACY_TOOLS: &[&str] = &[
    "todo__add",
    "todo__list",
    "todo__complete",
    "todo__update",
    "todo__remove",
];

/// Register the compact entry point and legacy handlers for saved sessions.
pub fn register_todo_tools(
    registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry,
    backend: std::sync::Arc<HybridTodoBackend>,
) {
    registry.register(TodoTool::new(backend.clone()));
    registry.register(TodoAddTool::new(backend.clone()));
    registry.register(TodoListTool::new(backend.clone()));
    registry.register(TodoCompleteTool::new(backend.clone()));
    registry.register(TodoUpdateTool::new(backend.clone()));
    registry.register(TodoRemoveTool::new(backend));
}

#[derive(Debug, Deserialize)]
struct TodoAddResult {
    items: Vec<TodoAddResultItem>,
}

#[derive(Debug, Deserialize)]
struct TodoAddResultItem {
    id: u64,
    title: String,
}

/// Produce `TodoEvent`s for mutation tools (add/complete/update/remove).
pub fn make_todo_events(tc: &ParsedToolCall, result_str: &str) -> Vec<TodoEvent> {
    if tc.name == "todo" {
        let Some(action) = tc.arguments["action"].as_str() else {
            return vec![];
        };
        if !["add", "list", "complete", "update", "remove"].contains(&action) {
            return vec![];
        }
        let legacy = ParsedToolCall {
            id: tc.id.clone(),
            name: format!("todo__{action}"),
            arguments: tc.arguments.clone(),
        };
        return make_todo_events(&legacy, result_str);
    }
    match tc.name.as_str() {
        "todo__add" => serde_json::from_str::<TodoAddResult>(result_str)
            .map(|result| {
                result
                    .items
                    .into_iter()
                    .map(|item| TodoEvent::Added {
                        id: item.id,
                        content: item.title,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "todo__complete" if result_str.contains("marked as done") => {
            let mut ids = tc.arguments["ids"]
                .as_array()
                .map(|ids| ids.iter().filter_map(|id| id.as_u64()).collect::<Vec<_>>())
                .unwrap_or_default();
            if let Some(id) = tc.arguments["id"].as_u64() {
                ids.push(id);
            }
            ids.sort_unstable();
            ids.dedup();
            ids.into_iter()
                .map(|id| TodoEvent::Completed { id })
                .collect()
        }
        "todo__update" if result_str.starts_with("Updated todo #") => {
            match (
                tc.arguments["id"].as_u64(),
                tc.arguments["content"].as_str(),
            ) {
                (Some(id), Some(content)) => vec![TodoEvent::Updated {
                    id,
                    content: content.to_string(),
                }],
                _ => Vec::new(),
            }
        }
        "todo__remove" if result_str.starts_with("Removed todo #") => tc.arguments["id"]
            .as_u64()
            .map(|id| vec![TodoEvent::Removed { id }])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::make_todo_events;
    use crate::events::TodoEvent;
    use remi_agentloop::prelude::ParsedToolCall;
    use serde_json::json;

    #[test]
    fn unified_and_legacy_batch_completion_emit_each_successful_item_once() {
        for name in ["todo", "todo__complete"] {
            let tc = ParsedToolCall {
                id: "complete".into(),
                name: name.into(),
                arguments: json!({"action":"complete","ids":[2,1,2]}),
            };
            assert!(make_todo_events(&tc, "Todo #99 not found.").is_empty());
            let events =
                make_todo_events(&tc, "Todos #1, #2 marked as done; completed batch cleared.");
            assert_eq!(events.len(), 2);
            assert!(matches!(events[0], TodoEvent::Completed { id: 1 }));
            assert!(matches!(events[1], TodoEvent::Completed { id: 2 }));
        }
    }

    #[tokio::test]
    async fn unified_todo_lifecycle_preserves_batches_and_events() {
        use futures::StreamExt;
        use remi_agentloop::prelude::{Tool, ToolOutput, ToolResult};
        use std::sync::Arc;
        let tool = super::TodoTool::new(Arc::new(super::HybridTodoBackend::new(
            std::path::PathBuf::from("/tmp"),
        )));
        let ctx = bot_runtime_core::ToolContext::default();
        for (args, contains, event_count) in [
            (
                json!({"action":"add","title":"Plan","items":[{"title":"One"},{"title":"Two"}]}),
                "batch_id",
                2,
            ),
            (
                json!({"action":"update","id":1,"content":"Renamed"}),
                "Renamed",
                1,
            ),
            (json!({"action":"list"}), "Renamed", 0),
            (
                json!({"action":"complete","ids":[1,2]}),
                "completed batch cleared",
                2,
            ),
            (
                json!({"action":"add","title":"Next","items":[{"title":"Third"}]}),
                "batch_id",
                1,
            ),
            (json!({"action":"remove","id":3}), "Removed todo #3", 1),
        ] {
            let tc = ParsedToolCall {
                id: "case".into(),
                name: "todo".into(),
                arguments: args.clone(),
            };
            let ToolResult::Output(output) = tool.execute(args, None, ctx.clone()).await.unwrap()
            else {
                panic!("unexpected interrupt")
            };
            let mut output = std::pin::pin!(output);
            let mut text = String::new();
            while let Some(item) = output.next().await {
                if let ToolOutput::Result(c) = item {
                    text = c.text_content();
                }
            }
            assert!(text.contains(contains), "{text}");
            assert_eq!(make_todo_events(&tc, &text).len(), event_count);
        }
        assert!(super::todos_from_user_state(&ctx.user_state()).is_empty());
        assert!(tool
            .execute(json!({"action":"bad"}), None, ctx)
            .await
            .is_err());
    }

    #[test]
    fn todo_add_emits_one_event_per_created_item() {
        let tc = ParsedToolCall {
            id: "call-1".to_string(),
            name: "todo__add".to_string(),
            arguments: json!({
                "title": "Release launch",
                "items": [
                    { "title": "Draft changelog" },
                    { "title": "Publish notes", "description": "Customer facing" }
                ]
            }),
        };
        let result = serde_json::to_string_pretty(&json!({
            "batch_id": 8,
            "batch_title": "Release launch",
            "todo_ids": [8, 9],
            "items": [
                { "id": 8, "title": "Draft changelog" },
                { "id": 9, "title": "Publish notes", "description": "Customer facing" }
            ]
        }))
        .expect("result JSON should serialize");

        let events = make_todo_events(&tc, &result);
        assert_eq!(events.len(), 2);
        match &events[0] {
            TodoEvent::Added { id, content } => {
                assert_eq!(*id, 8);
                assert_eq!(content, "Draft changelog");
            }
            other => panic!("expected add event, got {other:?}"),
        }
        match &events[1] {
            TodoEvent::Added { id, content } => {
                assert_eq!(*id, 9);
                assert_eq!(content, "Publish notes");
            }
            other => panic!("expected add event, got {other:?}"),
        }
    }
}
