//! Todo tools: add, list, complete, update, remove.
//!
//! State lives in `ctx.user_state["__todos"]` as a JSON array so it is
//! automatically serialised into every `AgentState` checkpoint.

use async_stream::stream;
use futures::Stream;
use remi_core::error::AgentError;
use remi_core::tool::{Tool, ToolContext, ToolOutput, ToolResult};
use remi_core::types::ResumePayload;
use serde::{Deserialize, Serialize};
use serde_json::json;

// ── Todo item ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: u64,
    pub content: String,
    pub done: bool,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Read the todo list from user_state, defaulting to empty.
fn read_todos(ctx: &ToolContext) -> Vec<TodoItem> {
    let us = ctx.user_state.read().unwrap();
    match us.get("__todos") {
        Some(v) => serde_json::from_value(v.clone()).unwrap_or_default(),
        None => vec![],
    }
}

/// Write the todo list back to user_state.
fn write_todos(ctx: &ToolContext, todos: Vec<TodoItem>) {
    let mut us = ctx.user_state.write().unwrap();
    us["__todos"] = serde_json::to_value(&todos).unwrap_or(json!([]));
}

/// Atomically modify the todo list under a single write lock to prevent
/// interleaving when multiple todo tools run in parallel.
///
/// `f` receives the current list and returns `(updated_list, return_value)`.
fn modify_todos<T>(ctx: &ToolContext, f: impl FnOnce(Vec<TodoItem>) -> (Vec<TodoItem>, T)) -> T {
    let mut us = ctx.user_state.write().unwrap();
    let todos: Vec<TodoItem> = match us.get("__todos") {
        Some(v) => serde_json::from_value(v.clone()).unwrap_or_default(),
        None => vec![],
    };
    let (updated, ret) = f(todos);
    us["__todos"] = serde_json::to_value(&updated).unwrap_or(json!([]));
    ret
}

/// Next ID = max existing + 1 (or 1 if empty).
fn next_id(todos: &[TodoItem]) -> u64 {
    todos.iter().map(|t| t.id).max().unwrap_or(0) + 1
}

fn fmt_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "No todos.".to_string();
    }
    todos
        .iter()
        .map(|t| {
            let mark = if t.done { "✓" } else { "○" };
            format!("[{}] {} {}", mark, t.id, t.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── TodoAddTool ───────────────────────────────────────────────────────────────

/// Add a new todo item to the todo list. Returns the new item ID.
pub struct TodoAddTool;

impl Tool for TodoAddTool {
    fn name(&self) -> &str {
        "todo__add"
    }
    fn description(&self) -> &str {
        "Add a new todo item. Returns the assigned numeric ID."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The todo item text" }
            },
            "required": ["content"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let content = arguments["content"]
            .as_str()
            .ok_or_else(|| AgentError::tool("todo__add", "missing 'content'"))?
            .to_string();

        let (id, content2) = modify_todos(ctx, |mut todos| {
            let id = next_id(&todos);
            todos.push(TodoItem {
                id,
                content: content.clone(),
                done: false,
            });
            (todos, (id, content))
        });
        let (id, content) = (id, content2);

        Ok(ToolResult::Output(stream! {
            yield ToolOutput::text(format!("Added todo #{id}: {content}"));
        }))
    }
}

// ── TodoListTool ──────────────────────────────────────────────────────────────

/// List all todo items with their completion status.
pub struct TodoListTool;

impl Tool for TodoListTool {
    fn name(&self) -> &str {
        "todo__list"
    }
    fn description(&self) -> &str {
        "List all todo items with their completion status."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let todos = read_todos(ctx);
        let text = fmt_todos(&todos);
        Ok(ToolResult::Output(stream! {
            yield ToolOutput::text(text);
        }))
    }
}

// ── TodoCompleteTool ──────────────────────────────────────────────────────────

/// Mark a todo item as done.
pub struct TodoCompleteTool;

impl Tool for TodoCompleteTool {
    fn name(&self) -> &str {
        "todo__complete"
    }
    fn description(&self) -> &str {
        "Mark a todo item as completed by its ID."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The todo item ID to mark as done" }
            },
            "required": ["id"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let id = arguments["id"]
            .as_u64()
            .ok_or_else(|| AgentError::tool("todo__complete", "missing 'id'"))?;

        let msg = modify_todos(ctx, |mut todos| {
            let msg = match todos.iter_mut().find(|t| t.id == id) {
                Some(t) => {
                    t.done = true;
                    format!("Todo #{id} marked as done.")
                }
                None => format!("Todo #{id} not found."),
            };
            (todos, msg)
        });
        Ok(ToolResult::Output(stream! { yield ToolOutput::text(msg); }))
    }
}

// ── TodoUpdateTool ────────────────────────────────────────────────────────────

/// Update the text of an existing todo item.
pub struct TodoUpdateTool;

impl Tool for TodoUpdateTool {
    fn name(&self) -> &str {
        "todo__update"
    }
    fn description(&self) -> &str {
        "Update the content text of an existing todo item by its ID."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id":      { "type": "integer", "description": "The todo item ID to update" },
                "content": { "type": "string",  "description": "New text for the todo item" }
            },
            "required": ["id", "content"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let id = arguments["id"]
            .as_u64()
            .ok_or_else(|| AgentError::tool("todo__update", "missing 'id'"))?;
        let content = arguments["content"]
            .as_str()
            .ok_or_else(|| AgentError::tool("todo__update", "missing 'content'"))?
            .to_string();

        let msg = modify_todos(ctx, |mut todos| {
            let msg = match todos.iter_mut().find(|t| t.id == id) {
                Some(t) => {
                    t.content = content.clone();
                    format!("Updated todo #{id}: {content}")
                }
                None => format!("Todo #{id} not found."),
            };
            (todos, msg)
        });
        Ok(ToolResult::Output(stream! { yield ToolOutput::text(msg); }))
    }
}

// ── TodoRemoveTool ────────────────────────────────────────────────────────────

/// Remove a todo item by ID.
pub struct TodoRemoveTool;

impl Tool for TodoRemoveTool {
    fn name(&self) -> &str {
        "todo__remove"
    }
    fn description(&self) -> &str {
        "Permanently remove a todo item by its ID."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "The todo item ID to delete" }
            },
            "required": ["id"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let id = arguments["id"]
            .as_u64()
            .ok_or_else(|| AgentError::tool("todo__remove", "missing 'id'"))?;

        let removed = modify_todos(ctx, |mut todos| {
            let before = todos.len();
            todos.retain(|t| t.id != id);
            let removed = before != todos.len();
            (todos, removed)
        });

        Ok(ToolResult::Output(stream! {
            if removed {
                yield ToolOutput::text(format!("Removed todo #{id}."));
            } else {
                yield ToolOutput::text(format!("Todo #{id} not found."));
            }
        }))
    }
}

/// All five todo tools as a convenience group.
pub struct TodoToolkit;

impl TodoToolkit {
    pub fn add(&self) -> TodoAddTool {
        TodoAddTool
    }
    pub fn list(&self) -> TodoListTool {
        TodoListTool
    }
    pub fn complete(&self) -> TodoCompleteTool {
        TodoCompleteTool
    }
    pub fn update(&self) -> TodoUpdateTool {
        TodoUpdateTool
    }
    pub fn remove(&self) -> TodoRemoveTool {
        TodoRemoveTool
    }
}
