use std::{collections::HashSet, path::PathBuf};

use anyhow::Result;
use bot_runtime_core::ToolContext;
use remi_agentloop::prelude::AgentError;
use serde_json::{json, Value};

use super::tools::{
    add_batch_to_todos, todos_from_user_state, write_todos_to_user_state, TodoBatchAddRequest,
    TodoBatchAddResult, TodoItem, TODOS_STATE_KEY, TODO_NEXT_ID_STATE_KEY,
};

pub struct HybridTodoBackend;

impl HybridTodoBackend {
    pub fn new(_data_dir: PathBuf) -> Self {
        Self
    }

    pub async fn refresh_thread_user_state(
        &self,
        _thread_id: &str,
        _user_id: Option<&str>,
        user_state: &mut Value,
    ) -> Result<()> {
        prune_stale_todos(user_state);
        Ok(())
    }

    pub async fn delete_thread(&self, _thread_id: &str, _user_id: Option<&str>) -> Result<()> {
        Ok(())
    }

    pub async fn fork_thread_user_state(
        &self,
        _source_thread_id: &str,
        _target_thread_id: &str,
        _user_id: Option<&str>,
        target_user_state: &mut Value,
    ) -> Result<()> {
        prune_stale_todos(target_user_state);
        Ok(())
    }

    pub(crate) async fn add_batch(
        &self,
        ctx: ToolContext,
        request: TodoBatchAddRequest,
    ) -> Result<TodoBatchAddResult, AgentError> {
        let mut user_state = self.load_user_state(&ctx).await;
        let todos = todos_from_user_state(&user_state);
        let next_id = user_state
            .get(TODO_NEXT_ID_STATE_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let (updated, result) = add_batch_to_todos(todos, request, next_id);
        write_todos_to_user_state(&mut user_state, &updated);
        user_state[TODO_NEXT_ID_STATE_KEY] = json!(result.todo_ids.last().unwrap() + 1);
        store_user_state(ctx, user_state);
        Ok(result)
    }

    pub async fn list(&self, ctx: ToolContext) -> Vec<TodoItem> {
        let user_state = self.load_user_state(&ctx).await;
        let todos = todos_from_user_state(&user_state);
        store_user_state(ctx, user_state);
        todos
    }

    pub async fn complete(&self, ctx: ToolContext, id: u64) -> Result<String, AgentError> {
        let mut user_state = self.load_user_state(&ctx).await;
        let mut todos = todos_from_user_state(&user_state);
        let Some(index) = todos.iter().position(|todo| todo.id == id) else {
            store_user_state(ctx, user_state);
            return Ok(format!("Todo #{id} not found."));
        };

        todos[index].done = true;
        write_todos_to_user_state(&mut user_state, &todos);
        let batch_id = todos[index].batch_id;
        prune_stale_todos(&mut user_state);
        let batch_cleared = batch_id.is_some()
            && !todos_from_user_state(&user_state)
                .iter()
                .any(|todo| todo.batch_id == batch_id);
        store_user_state(ctx, user_state);
        if batch_cleared {
            Ok(format!(
                "Todo #{id} marked as done; completed batch cleared."
            ))
        } else {
            Ok(format!("Todo #{id} marked as done."))
        }
    }

    pub async fn update(
        &self,
        ctx: ToolContext,
        id: u64,
        content: String,
    ) -> Result<String, AgentError> {
        let mut user_state = self.load_user_state(&ctx).await;
        let mut todos = todos_from_user_state(&user_state);
        let Some(index) = todos.iter().position(|todo| todo.id == id) else {
            store_user_state(ctx, user_state);
            return Ok(format!("Todo #{id} not found."));
        };

        todos[index].content = content.clone();
        write_todos_to_user_state(&mut user_state, &todos);
        store_user_state(ctx, user_state);
        Ok(format!("Updated todo #{id}: {content}"))
    }

    pub async fn remove(&self, ctx: ToolContext, id: u64) -> Result<String, AgentError> {
        let mut user_state = self.load_user_state(&ctx).await;
        let mut todos = todos_from_user_state(&user_state);
        let Some(index) = todos.iter().position(|todo| todo.id == id) else {
            store_user_state(ctx, user_state);
            return Ok(format!("Todo #{id} not found."));
        };

        todos.remove(index);
        write_todos_to_user_state(&mut user_state, &todos);
        prune_stale_todos(&mut user_state);
        store_user_state(ctx, user_state);
        Ok(format!("Removed todo #{id}."))
    }

    async fn load_user_state(&self, ctx: &ToolContext) -> Value {
        let mut user_state = ctx.user_state();
        prune_stale_todos(&mut user_state);
        user_state
    }
}

fn store_user_state(ctx: ToolContext, user_state: Value) {
    ctx.set_user_state(user_state);
}

fn prune_stale_todos(user_state: &mut Value) {
    if user_state.get(TODOS_STATE_KEY).is_none() {
        return;
    }
    let mut todos = todos_from_user_state(user_state);
    let active_batches: HashSet<u64> = todos
        .iter()
        .filter(|todo| !todo.done)
        .filter_map(|todo| todo.batch_id)
        .collect();
    let completed_batches: HashSet<u64> = todos
        .iter()
        .filter_map(|todo| todo.batch_id)
        .filter(|id| !active_batches.contains(id))
        .collect();
    if !completed_batches.is_empty() {
        let next_id = todos.iter().map(|todo| todo.id).max().unwrap_or(0) + 1;
        let existing = user_state
            .get(TODO_NEXT_ID_STATE_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(1);
        user_state[TODO_NEXT_ID_STATE_KEY] = json!(existing.max(next_id));
        todos.retain(|todo| {
            !todo
                .batch_id
                .is_some_and(|id| completed_batches.contains(&id))
        });
    }
    write_todos_to_user_state(user_state, &todos);
}

#[cfg(test)]
mod tests {
    use bot_runtime_core::ToolContext;
    use remi_agentloop::prelude::{RunId, ThreadId};
    use serde_json::json;
    use std::path::PathBuf;

    use super::HybridTodoBackend;
    use crate::todo::tools::{
        todos_from_user_state, write_todos_to_user_state, TodoBatchAddItemRequest,
        TodoBatchAddRequest, TodoItem,
    };

    fn test_ctx(user_state: serde_json::Value) -> ToolContext {
        ToolContext::with_ids(
            ThreadId("thread-1".to_string()),
            RunId("run-1".to_string()),
            bot_runtime_core::ChatCtxState {
                user_state,
                ..bot_runtime_core::ChatCtxState::default()
            },
        )
    }

    fn backend() -> HybridTodoBackend {
        HybridTodoBackend::new(PathBuf::from("/tmp/remi-cat-test"))
    }

    fn todo(id: u64, content: &str) -> TodoItem {
        TodoItem {
            id,
            content: content.to_string(),
            description: None,
            done: false,
            batch_id: None,
            batch_title: None,
            batch_index: None,
        }
    }

    #[tokio::test]
    async fn local_backend_mutates_user_state() {
        let backend = backend();
        let ctx = test_ctx(serde_json::Value::Null);
        let result = backend
            .add_batch(
                ctx.clone(),
                TodoBatchAddRequest {
                    title: "Release".to_string(),
                    items: vec![
                        TodoBatchAddItemRequest {
                            title: "Draft changelog".to_string(),
                            description: None,
                        },
                        TodoBatchAddItemRequest {
                            title: "Publish notes".to_string(),
                            description: Some("Customer-facing".to_string()),
                        },
                    ],
                },
            )
            .await
            .expect("add should succeed");

        assert_eq!(result.todo_ids, vec![1, 2]);
        assert_eq!(
            backend.complete(ctx.clone(), 1).await.unwrap(),
            "Todo #1 marked as done."
        );
        assert_eq!(
            backend
                .update(ctx.clone(), 2, "Publish release notes".to_string())
                .await
                .unwrap(),
            "Updated todo #2: Publish release notes"
        );
        assert_eq!(
            backend.remove(ctx.clone(), 1).await.unwrap(),
            "Removed todo #1."
        );

        let user_state = ctx.user_state();
        let todos = todos_from_user_state(&user_state);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].id, 2);
        assert_eq!(todos[0].content, "Publish release notes");
    }

    #[tokio::test]
    async fn completing_last_item_clears_only_its_batch_and_keeps_ids_monotonic() {
        let backend = backend();
        let ctx = test_ctx(serde_json::Value::Null);
        let first = backend
            .add_batch(
                ctx.clone(),
                TodoBatchAddRequest {
                    title: "First".into(),
                    items: vec![
                        TodoBatchAddItemRequest {
                            title: "A".into(),
                            description: None,
                        },
                        TodoBatchAddItemRequest {
                            title: "B".into(),
                            description: None,
                        },
                    ],
                },
            )
            .await
            .unwrap();
        let second = backend
            .add_batch(
                ctx.clone(),
                TodoBatchAddRequest {
                    title: "Second".into(),
                    items: vec![TodoBatchAddItemRequest {
                        title: "C".into(),
                        description: None,
                    }],
                },
            )
            .await
            .unwrap();

        backend
            .complete(ctx.clone(), first.todo_ids[0])
            .await
            .unwrap();
        assert_eq!(backend.list(ctx.clone()).await.len(), 3);
        assert_eq!(
            backend
                .complete(ctx.clone(), first.todo_ids[1])
                .await
                .unwrap(),
            "Todo #2 marked as done; completed batch cleared."
        );
        assert_eq!(
            backend
                .list(ctx.clone())
                .await
                .iter()
                .map(|todo| todo.id)
                .collect::<Vec<_>>(),
            second.todo_ids
        );
        backend
            .complete(ctx.clone(), second.todo_ids[0])
            .await
            .unwrap();
        assert!(backend.list(ctx.clone()).await.is_empty());

        let next = backend
            .add_batch(
                ctx.clone(),
                TodoBatchAddRequest {
                    title: "Next".into(),
                    items: vec![TodoBatchAddItemRequest {
                        title: "D".into(),
                        description: None,
                    }],
                },
            )
            .await
            .unwrap();
        assert_eq!(next.todo_ids, vec![4]);
    }

    #[tokio::test]
    async fn refresh_clears_previously_completed_batches() {
        let backend = backend();
        let mut user_state = json!({
            "__todos": [
                {"id": 1, "content": "Done A", "done": true, "batch_id": 1},
                {"id": 2, "content": "Done B", "done": true, "batch_id": 1},
                {"id": 3, "content": "Done C", "done": true, "batch_id": 3},
                {"id": 4, "content": "Open D", "done": false, "batch_id": 3},
                {"id": 5, "content": "Legacy", "done": true}
            ],
            "other": "preserved"
        });
        backend
            .refresh_thread_user_state("thread-1", None, &mut user_state)
            .await
            .unwrap();
        assert_eq!(
            todos_from_user_state(&user_state)
                .iter()
                .map(|todo| todo.id)
                .collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(user_state["__todo_next_id"], 6);
        assert_eq!(user_state["other"], "preserved");
    }

    #[tokio::test]
    async fn removing_last_open_item_clears_finished_remainder() {
        let backend = backend();
        let ctx = test_ctx(serde_json::Value::Null);
        let batch = backend
            .add_batch(
                ctx.clone(),
                TodoBatchAddRequest {
                    title: "Plan".into(),
                    items: vec![
                        TodoBatchAddItemRequest {
                            title: "Done".into(),
                            description: None,
                        },
                        TodoBatchAddItemRequest {
                            title: "Cancelled".into(),
                            description: None,
                        },
                    ],
                },
            )
            .await
            .unwrap();
        backend
            .complete(ctx.clone(), batch.todo_ids[0])
            .await
            .unwrap();
        backend
            .remove(ctx.clone(), batch.todo_ids[1])
            .await
            .unwrap();
        assert!(backend.list(ctx).await.is_empty());
    }

    #[tokio::test]
    async fn refresh_prunes_legacy_sdk_todos() {
        let backend = backend();
        let mut user_state = json!({
            "__todos": [
                {"id": 1, "content": "Local", "done": false},
                {
                    "id": 2,
                    "content": "SDK",
                    "done": false,
                    "storage_kind": "remi_sdk",
                    "collection_uuid": "collection-1",
                    "thing_uuid": "thing-1"
                }
            ]
        });

        backend
            .refresh_thread_user_state("thread-1", Some("user-1"), &mut user_state)
            .await
            .expect("refresh should succeed");

        assert_eq!(todos_from_user_state(&user_state), vec![todo(1, "Local")]);
        assert!(user_state["__todos"][0].get("storage_kind").is_none());
    }

    #[tokio::test]
    async fn fork_prunes_legacy_sdk_todos_in_target_state() {
        let backend = backend();
        let mut user_state = json!({
            "__todos": [
                {"id": 1, "content": "Local", "done": false},
                {"id": 2, "content": "SDK", "done": false, "storage_kind": "remi_sdk"}
            ]
        });

        backend
            .fork_thread_user_state("source", "target", None, &mut user_state)
            .await
            .expect("fork should succeed");

        assert_eq!(todos_from_user_state(&user_state), vec![todo(1, "Local")]);
    }

    #[tokio::test]
    async fn delete_thread_is_noop() {
        backend()
            .delete_thread("thread-1", Some("user-1"))
            .await
            .expect("delete should succeed");
    }

    #[test]
    fn write_todos_keeps_local_shape() {
        let mut user_state = serde_json::Value::Null;
        write_todos_to_user_state(&mut user_state, &[todo(1, "Local")]);

        assert_eq!(
            user_state,
            json!({
                "__todos": [
                    {
                        "id": 1,
                        "content": "Local",
                        "done": false
                    }
                ]
            })
        );
    }
}
