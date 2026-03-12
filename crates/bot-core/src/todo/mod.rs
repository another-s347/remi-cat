pub mod tools;

pub use tools::{TodoAddTool, TodoCompleteTool, TodoListTool, TodoRemoveTool, TodoUpdateTool};

use remi_agentloop::prelude::ParsedToolCall;

use crate::events::TodoEvent;

/// Register all five todo tools into a registry.
pub fn register_todo_tools(registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry) {
    registry.register(TodoAddTool);
    registry.register(TodoListTool);
    registry.register(TodoCompleteTool);
    registry.register(TodoUpdateTool);
    registry.register(TodoRemoveTool);
}

/// Produce a `TodoEvent` for mutation tools (add/complete/update/remove).
pub fn make_todo_event(tc: &ParsedToolCall, result_str: &str) -> Option<TodoEvent> {
    match tc.name.as_str() {
        "todo__add" => {
            // result is "Added todo #N: content"
            let id = result_str
                .strip_prefix("Added todo #")
                .and_then(|s| s.split(':').next())
                .and_then(|s| s.trim().parse::<u64>().ok())?;
            let content = tc.arguments["content"].as_str()?.to_string();
            Some(TodoEvent::Added { id, content })
        }
        "todo__complete" => {
            let id = tc.arguments["id"].as_u64()?;
            Some(TodoEvent::Completed { id })
        }
        "todo__update" => {
            let id = tc.arguments["id"].as_u64()?;
            let content = tc.arguments["content"].as_str()?.to_string();
            Some(TodoEvent::Updated { id, content })
        }
        "todo__remove" => {
            let id = tc.arguments["id"].as_u64()?;
            Some(TodoEvent::Removed { id })
        }
        _ => None,
    }
}
