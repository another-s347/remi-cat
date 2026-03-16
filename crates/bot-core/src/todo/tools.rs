//! Todo tools: add, list, complete, update, remove.
//!
//! State lives in `ctx.user_state["__todos"]` as a JSON array so it is
//! automatically serialised into every `AgentState` checkpoint.

use std::collections::HashMap;
use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{AgentError, Tool, ToolContext, ToolOutput, ToolResult};
use remi_agentloop::types::ResumePayload;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::backend::HybridTodoBackend;

pub(crate) const TODOS_STATE_KEY: &str = "__todos";
const UNGROUPED_SECTION_TITLE: &str = "Ungrouped";
const TODO_BATCH_PROMPT_HEADER: &str = "[CURRENT TODO BATCH]";
const TODO_BATCH_EXECUTION_GUIDANCE: &str = "When this thread has an active plan, try to complete multiple todo items in one pass whenever feasible. Only stop early if the user explicitly cancels, changes direction, or you need user input/help to proceed. Finish each individual todo item's work before marking it complete.";

// ── Todo item ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStorageKind {
    Simple,
    RemiSdk,
}

impl Default for TodoStorageKind {
    fn default() -> Self {
        Self::Simple
    }
}

impl TodoStorageKind {
    pub fn is_simple(&self) -> bool {
        matches!(self, Self::Simple)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: u64,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_index: Option<u64>,
    #[serde(default, skip_serializing_if = "TodoStorageKind::is_simple")]
    pub storage_kind: TodoStorageKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thing_uuid: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TodoBatchAddRequest {
    title: String,
    items: Vec<TodoBatchAddItemRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TodoBatchAddItemRequest {
    title: String,
    description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TodoBatchAddResult {
    pub(crate) batch_id: u64,
    pub(crate) batch_title: String,
    pub(crate) todo_ids: Vec<u64>,
    pub(crate) items: Vec<TodoBatchAddResultItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TodoBatchAddResultItem {
    id: u64,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTodoBatchAddRequest {
    title: String,
    items: Vec<RawTodoBatchAddItemRequest>,
}

#[derive(Debug, Deserialize)]
struct RawTodoBatchAddItemRequest {
    title: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TodoBatchGroup {
    batch_id: u64,
    title: String,
    items: Vec<TodoItem>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

pub(crate) fn todos_from_user_state(user_state: &serde_json::Value) -> Vec<TodoItem> {
    match user_state.get(TODOS_STATE_KEY) {
        Some(v) => serde_json::from_value(v.clone()).unwrap_or_default(),
        None => vec![],
    }
}

pub(crate) fn write_todos_to_user_state(user_state: &mut serde_json::Value, todos: &[TodoItem]) {
    if !user_state.is_object() {
        *user_state = json!({});
    }
    if let Some(map) = user_state.as_object_mut() {
        map.insert(
            TODOS_STATE_KEY.to_string(),
            serde_json::to_value(todos).unwrap_or(json!([])),
        );
    }
}

fn next_id(todos: &[TodoItem]) -> u64 {
    todos.iter().map(|t| t.id).max().unwrap_or(0) + 1
}

fn trim_to_option(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn normalize_required_text(
    tool_name: &str,
    field_name: &str,
    value: &str,
) -> Result<String, AgentError> {
    trim_to_option(Some(value))
        .ok_or_else(|| AgentError::tool(tool_name, format!("missing '{field_name}'")))
}

fn parse_add_request(arguments: serde_json::Value) -> Result<TodoBatchAddRequest, AgentError> {
    let raw: RawTodoBatchAddRequest = serde_json::from_value(arguments)
        .map_err(|_| AgentError::tool("todo__add", "expected {title, items[]}"))?;
    let title = normalize_required_text("todo__add", "title", &raw.title)?;
    if raw.items.is_empty() {
        return Err(AgentError::tool(
            "todo__add",
            "items must contain at least one todo item",
        ));
    }

    let mut items = Vec::with_capacity(raw.items.len());
    for (index, item) in raw.items.into_iter().enumerate() {
        let field_name = format!("items[{index}].title");
        let item_title = normalize_required_text("todo__add", &field_name, &item.title)?;
        items.push(TodoBatchAddItemRequest {
            title: item_title,
            description: trim_to_option(item.description.as_deref()),
        });
    }

    Ok(TodoBatchAddRequest { title, items })
}

pub(crate) fn add_batch_to_todos(
    mut todos: Vec<TodoItem>,
    request: TodoBatchAddRequest,
) -> (Vec<TodoItem>, TodoBatchAddResult) {
    let batch_title = request.title;
    let first_id = next_id(&todos);
    let batch_id = first_id;
    let mut todo_ids = Vec::with_capacity(request.items.len());
    let mut result_items = Vec::with_capacity(request.items.len());

    for (index, item) in request.items.into_iter().enumerate() {
        let id = first_id + index as u64;
        todo_ids.push(id);
        result_items.push(TodoBatchAddResultItem {
            id,
            title: item.title.clone(),
            description: item.description.clone(),
        });
        todos.push(TodoItem {
            id,
            content: item.title,
            description: item.description,
            done: false,
            batch_id: Some(batch_id),
            batch_title: Some(batch_title.clone()),
            batch_index: Some(index as u64),
            storage_kind: TodoStorageKind::Simple,
            collection_uuid: None,
            thing_uuid: None,
        });
    }

    (
        todos,
        TodoBatchAddResult {
            batch_id,
            batch_title,
            todo_ids,
            items: result_items,
        },
    )
}

fn grouped_todos(todos: &[TodoItem]) -> (Vec<TodoBatchGroup>, Vec<TodoItem>) {
    let mut batches: Vec<TodoBatchGroup> = Vec::new();
    let mut batch_positions: HashMap<u64, usize> = HashMap::new();
    let mut ungrouped = Vec::new();

    for todo in todos.iter().cloned() {
        let Some(batch_id) = todo.batch_id else {
            ungrouped.push(todo);
            continue;
        };

        let batch_title = trim_to_option(todo.batch_title.as_deref())
            .unwrap_or_else(|| format!("Batch {batch_id}"));

        let position = match batch_positions.get(&batch_id).copied() {
            Some(position) => {
                if batches[position].title == format!("Batch {batch_id}") {
                    batches[position].title = batch_title.clone();
                }
                position
            }
            None => {
                let position = batches.len();
                batches.push(TodoBatchGroup {
                    batch_id,
                    title: batch_title,
                    items: Vec::new(),
                });
                batch_positions.insert(batch_id, position);
                position
            }
        };
        batches[position].items.push(todo);
    }

    for batch in &mut batches {
        batch
            .items
            .sort_by_key(|todo| (todo.batch_index.unwrap_or(u64::MAX), todo.id));
    }

    (batches, ungrouped)
}

fn format_todo_line(todo: &TodoItem, indent: &str) -> String {
    let mark = if todo.done { "✓" } else { "○" };
    let mut lines = vec![format!("{indent}[{mark}] {} {}", todo.id, todo.content)];
    if let Some(description) = trim_to_option(todo.description.as_deref()) {
        lines.push(format!("{indent}    {description}"));
    }
    lines.join("\n")
}

fn fmt_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "No todos.".to_string();
    }

    let (batches, ungrouped) = grouped_todos(todos);
    let mut sections = Vec::new();

    for batch in batches {
        let mut lines = vec![format!("[Batch] {}", batch.title)];
        for todo in &batch.items {
            lines.push(format_todo_line(todo, "  "));
        }
        sections.push(lines.join("\n"));
    }

    if !ungrouped.is_empty() {
        let mut lines = vec![format!("[{UNGROUPED_SECTION_TITLE}]")];
        for todo in &ungrouped {
            lines.push(format_todo_line(todo, "  "));
        }
        sections.push(lines.join("\n"));
    }

    sections.join("\n\n")
}

pub fn latest_unfinished_batch_system_prompt(user_state: &serde_json::Value) -> Option<String> {
    let todos = todos_from_user_state(user_state);
    let (batches, _) = grouped_todos(&todos);
    let batch = batches
        .into_iter()
        .rev()
        .find(|batch| batch.items.iter().any(|todo| !todo.done))?;

    let mut lines = vec![
        TODO_BATCH_PROMPT_HEADER.to_string(),
        format!(
            "This thread still has unfinished work under \"{}\".",
            batch.title
        ),
        "Keep progress synchronized with todo__complete/update/remove.".to_string(),
        TODO_BATCH_EXECUTION_GUIDANCE.to_string(),
    ];
    for todo in batch.items.into_iter().filter(|todo| !todo.done) {
        let mut line = format!("- #{} {}", todo.id, todo.content);
        if let Some(description) = trim_to_option(todo.description.as_deref()) {
            line.push_str(" - ");
            line.push_str(&description);
        }
        lines.push(line);
    }

    Some(lines.join("\n"))
}

pub fn current_todo_card_markdown(user_state: &serde_json::Value) -> Option<String> {
    let todos = todos_from_user_state(user_state);
    if todos.is_empty() || todos.iter().all(|todo| todo.done) {
        return None;
    }

    Some(format!("📝 **当前 Todo**\n```\n{}\n```", fmt_todos(&todos)))
}

// ── TodoAddTool ───────────────────────────────────────────────────────────────

pub struct TodoAddTool {
    backend: Arc<HybridTodoBackend>,
}

impl TodoAddTool {
    pub fn new(backend: Arc<HybridTodoBackend>) -> Self {
        Self { backend }
    }
}

impl Tool for TodoAddTool {
    fn name(&self) -> &str {
        "todo__add"
    }
    fn description(&self) -> &str {
        "Add a new batch of todo items under a shared title. Returns the created todo IDs."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "The overall title for this batch of todos"
                },
                "items": {
                    "type": "array",
                    "description": "The child todos to create under the shared title",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {
                                "type": "string",
                                "description": "The child todo title"
                            },
                            "description": {
                                "type": "string",
                                "description": "Optional extra detail for the child todo"
                            }
                        },
                        "required": ["title"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["title", "items"],
            "additionalProperties": false
        })
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let request = parse_add_request(arguments)?;
        let result = self.backend.add_batch(ctx, request).await?;
        Ok(ToolResult::Output(stream! {
            let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| {
                json!({
                    "batch_id": result.batch_id,
                    "batch_title": result.batch_title,
                    "todo_ids": result.todo_ids,
                })
                .to_string()
            });
            yield ToolOutput::text(text);
        }))
    }
}

// ── TodoListTool ──────────────────────────────────────────────────────────────

pub struct TodoListTool {
    backend: Arc<HybridTodoBackend>,
}

impl TodoListTool {
    pub fn new(backend: Arc<HybridTodoBackend>) -> Self {
        Self { backend }
    }
}

impl Tool for TodoListTool {
    fn name(&self) -> &str {
        "todo__list"
    }
    fn description(&self) -> &str {
        "List all todo items with their completion status, grouped by batch title when available."
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
        let todos = self.backend.list(ctx).await;
        let text = fmt_todos(&todos);
        Ok(ToolResult::Output(
            stream! { yield ToolOutput::text(text); },
        ))
    }
}

// ── TodoCompleteTool ──────────────────────────────────────────────────────────

pub struct TodoCompleteTool {
    backend: Arc<HybridTodoBackend>,
}

impl TodoCompleteTool {
    pub fn new(backend: Arc<HybridTodoBackend>) -> Self {
        Self { backend }
    }
}

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
        let msg = self.backend.complete(ctx, id).await?;
        Ok(ToolResult::Output(stream! { yield ToolOutput::text(msg); }))
    }
}

// ── TodoUpdateTool ────────────────────────────────────────────────────────────

pub struct TodoUpdateTool {
    backend: Arc<HybridTodoBackend>,
}

impl TodoUpdateTool {
    pub fn new(backend: Arc<HybridTodoBackend>) -> Self {
        Self { backend }
    }
}

impl Tool for TodoUpdateTool {
    fn name(&self) -> &str {
        "todo__update"
    }
    fn description(&self) -> &str {
        "Update the title text of an existing todo item by its ID."
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
        let content = normalize_required_text(
            "todo__update",
            "content",
            arguments["content"]
                .as_str()
                .ok_or_else(|| AgentError::tool("todo__update", "missing 'content'"))?,
        )?;
        let msg = self.backend.update(ctx, id, content).await?;
        Ok(ToolResult::Output(stream! { yield ToolOutput::text(msg); }))
    }
}

// ── TodoRemoveTool ────────────────────────────────────────────────────────────

pub struct TodoRemoveTool {
    backend: Arc<HybridTodoBackend>,
}

impl TodoRemoveTool {
    pub fn new(backend: Arc<HybridTodoBackend>) -> Self {
        Self { backend }
    }
}

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
        let msg = self.backend.remove(ctx, id).await?;
        Ok(ToolResult::Output(stream! {
            yield ToolOutput::text(msg);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        add_batch_to_todos, current_todo_card_markdown, fmt_todos,
        latest_unfinished_batch_system_prompt, todos_from_user_state, write_todos_to_user_state,
        TodoBatchAddItemRequest, TodoBatchAddRequest, TodoItem, TodoStorageKind,
    };
    use serde_json::json;

    fn batch_request(title: &str, items: &[(&str, Option<&str>)]) -> TodoBatchAddRequest {
        TodoBatchAddRequest {
            title: title.to_string(),
            items: items
                .iter()
                .map(|(item_title, description)| TodoBatchAddItemRequest {
                    title: (*item_title).to_string(),
                    description: description.map(str::to_string),
                })
                .collect(),
        }
    }

    fn legacy_todo(id: u64, content: &str) -> TodoItem {
        TodoItem {
            id,
            content: content.to_string(),
            description: None,
            done: false,
            batch_id: None,
            batch_title: None,
            batch_index: None,
            storage_kind: TodoStorageKind::Simple,
            collection_uuid: None,
            thing_uuid: None,
        }
    }

    #[test]
    fn batch_add_assigns_ids_in_order() {
        let (todos, result) = add_batch_to_todos(
            vec![legacy_todo(4, "Existing todo")],
            batch_request(
                "Release launch",
                &[
                    ("Draft changelog", Some("Include breaking changes")),
                    ("Publish notes", None),
                ],
            ),
        );

        assert_eq!(result.batch_id, 5);
        assert_eq!(result.todo_ids, vec![5, 6]);
        assert_eq!(todos.len(), 3);
        assert_eq!(todos[1].batch_id, Some(5));
        assert_eq!(todos[1].batch_title.as_deref(), Some("Release launch"));
        assert_eq!(
            todos[1].description.as_deref(),
            Some("Include breaking changes")
        );
        assert_eq!(todos[2].batch_index, Some(1));
    }

    #[test]
    fn batch_add_stays_isolated_per_thread_state() {
        let mut thread_a = serde_json::Value::Null;
        let mut thread_b = serde_json::Value::Null;

        let (todos_a, result_a) = add_batch_to_todos(
            todos_from_user_state(&thread_a),
            batch_request("Thread A", &[("Task A1", None), ("Task A2", None)]),
        );
        write_todos_to_user_state(&mut thread_a, &todos_a);

        let (todos_b, result_b) = add_batch_to_todos(
            todos_from_user_state(&thread_b),
            batch_request("Thread B", &[("Task B1", Some("Only in B"))]),
        );
        write_todos_to_user_state(&mut thread_b, &todos_b);

        assert_eq!(result_a.todo_ids, vec![1, 2]);
        assert_eq!(result_b.todo_ids, vec![1]);
        assert_eq!(todos_from_user_state(&thread_a).len(), 2);
        assert_eq!(todos_from_user_state(&thread_b).len(), 1);
        assert_eq!(
            todos_from_user_state(&thread_b)[0].description.as_deref(),
            Some("Only in B")
        );
    }

    #[test]
    fn list_output_groups_batches_and_legacy_items() {
        let todos = vec![
            TodoItem {
                id: 1,
                content: "Draft changelog".to_string(),
                description: Some("Include breaking changes".to_string()),
                done: false,
                batch_id: Some(1),
                batch_title: Some("Release launch".to_string()),
                batch_index: Some(0),
                storage_kind: TodoStorageKind::Simple,
                collection_uuid: None,
                thing_uuid: None,
            },
            TodoItem {
                id: 2,
                content: "Publish notes".to_string(),
                description: None,
                done: true,
                batch_id: Some(1),
                batch_title: Some("Release launch".to_string()),
                batch_index: Some(1),
                storage_kind: TodoStorageKind::Simple,
                collection_uuid: None,
                thing_uuid: None,
            },
            legacy_todo(3, "Legacy follow-up"),
        ];

        assert_eq!(
            fmt_todos(&todos),
            "[Batch] Release launch\n  [○] 1 Draft changelog\n      Include breaking changes\n  [✓] 2 Publish notes\n\n[Ungrouped]\n  [○] 3 Legacy follow-up"
        );
    }

    #[test]
    fn latest_unfinished_prompt_uses_latest_open_batch() {
        let (todos, _) = add_batch_to_todos(
            vec![],
            batch_request("Phase 1", &[("Design schema", None), ("Write docs", None)]),
        );
        let (mut todos, _) = add_batch_to_todos(
            todos,
            batch_request(
                "Phase 2",
                &[("Ship backend", Some("Deploy first")), ("Announce", None)],
            ),
        );

        for todo in &mut todos {
            if todo.batch_title.as_deref() == Some("Phase 2") {
                todo.done = true;
            }
        }
        let phase_one_second = todos
            .iter_mut()
            .find(|todo| todo.content == "Write docs")
            .expect("phase 1 todo should exist");
        phase_one_second.done = true;

        let user_state = json!({
            "__todos": todos,
        });
        let prompt = latest_unfinished_batch_system_prompt(&user_state)
            .expect("prompt should exist for the latest unfinished batch");

        assert!(prompt.contains("\"Phase 1\""));
        assert!(prompt.contains("- #1 Design schema"));
        assert!(
            prompt.contains("try to complete multiple todo items in one pass whenever feasible")
        );
        assert!(
            prompt.contains("Finish each individual todo item's work before marking it complete")
        );
        assert!(!prompt.contains("Write docs"));
        assert!(!prompt.contains("Phase 2"));
    }

    #[test]
    fn legacy_ungrouped_todos_do_not_inject_batch_prompt() {
        let user_state = json!({
            "__todos": [legacy_todo(1, "Legacy follow-up")],
        });

        assert_eq!(latest_unfinished_batch_system_prompt(&user_state), None);
    }

    #[test]
    fn current_todo_card_markdown_formats_grouped_todos() {
        let user_state = json!({
            "__todos": [
                TodoItem {
                    id: 1,
                    content: "Draft changelog".to_string(),
                    description: Some("Include breaking changes".to_string()),
                    done: false,
                    batch_id: Some(1),
                    batch_title: Some("Release launch".to_string()),
                    batch_index: Some(0),
                    storage_kind: TodoStorageKind::Simple,
                    collection_uuid: None,
                    thing_uuid: None,
                },
                TodoItem {
                    id: 2,
                    content: "Legacy follow-up".to_string(),
                    description: None,
                    done: true,
                    batch_id: None,
                    batch_title: None,
                    batch_index: None,
                    storage_kind: TodoStorageKind::Simple,
                    collection_uuid: None,
                    thing_uuid: None,
                }
            ],
        });

        assert_eq!(
            current_todo_card_markdown(&user_state).as_deref(),
            Some(
                "📝 **当前 Todo**\n```\n[Batch] Release launch\n  [○] 1 Draft changelog\n      Include breaking changes\n\n[Ungrouped]\n  [✓] 2 Legacy follow-up\n```"
            )
        );
    }

    #[test]
    fn current_todo_card_markdown_omits_fully_completed_plans() {
        let user_state = json!({
            "__todos": [
                TodoItem {
                    id: 1,
                    content: "Draft changelog".to_string(),
                    description: None,
                    done: true,
                    batch_id: Some(1),
                    batch_title: Some("Release launch".to_string()),
                    batch_index: Some(0),
                    storage_kind: TodoStorageKind::Simple,
                    collection_uuid: None,
                    thing_uuid: None,
                },
                TodoItem {
                    id: 2,
                    content: "Publish notes".to_string(),
                    description: None,
                    done: true,
                    batch_id: Some(1),
                    batch_title: Some("Release launch".to_string()),
                    batch_index: Some(1),
                    storage_kind: TodoStorageKind::Simple,
                    collection_uuid: None,
                    thing_uuid: None,
                }
            ],
        });

        assert_eq!(current_todo_card_markdown(&user_state), None);
    }
}
