pub mod backend;
pub mod tools;

pub use backend::HybridTodoBackend;
pub use tools::{
    current_todo_card_markdown, latest_unfinished_batch_system_prompt, TodoAddTool,
    TodoCompleteTool, TodoListTool, TodoRemoveTool, TodoUpdateTool,
};

use remi_agentloop::prelude::ParsedToolCall;
use serde::Deserialize;

use crate::events::TodoEvent;

/// Register all five todo tools into a registry.
pub fn register_todo_tools(
    registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry,
    backend: std::sync::Arc<HybridTodoBackend>,
) {
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
        "todo__complete" => tc.arguments["id"]
            .as_u64()
            .map(|id| vec![TodoEvent::Completed { id }])
            .unwrap_or_default(),
        "todo__update" => {
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
        "todo__remove" => tc.arguments["id"]
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
