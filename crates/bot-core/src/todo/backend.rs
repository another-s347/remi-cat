use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use remi_agentloop::prelude::{AgentError, ToolContext};
use remi_client_sdk::auth::auth_set_app_key;
use remi_client_sdk::things_crdt::{ThingCollectionUpsert, ThingUpsert, ThingsSnapshotState};
use remi_client_sdk::things_events::ThingsEvent;
use remi_client_sdk::things_sync::{sync_v3_documents_with_server_mode, ThingsSyncMode};
use remi_client_sdk::transport::configure_shared_transport;
use remi_client_sdk::{TriggerClient, TriggerSdk};
use remi_things_crdt::{apply_collection_op, CollectionOp, ThingDatatype, TriggerUpdate};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::{timeout, Duration, Instant};
use tracing::{info, warn};
use uuid::Uuid;

use super::tools::{
    add_batch_to_todos, todos_from_user_state, write_todos_to_user_state, TodoBatchAddRequest,
    TodoBatchAddResult, TodoItem, TodoStorageKind,
};

const REMI_APP_KEY_ENV: &str = "REMI_APP_KEY";
const REMI_PUBLIC_GRPC_ADDR_ENV: &str = "REMI_PUBLIC_GRPC_ADDR";
const REMI_TODO_DEVICE_ID_ENV: &str = "REMI_TODO_DEVICE_ID";
const TODO_CREATE_VIA_SDK_META_KEY: &str = "todo_create_via_sdk";
const SDK_ATTRS_ROOT_KEY: &str = "remi_cat_todo";
const SDK_ATTRS_SOURCE: &str = "remi-cat-todo";
const SDK_DB_FILE_NAME: &str = "todo-sdk.db";
const SDK_LOCK_WARN_AFTER: Duration = Duration::from_millis(500);
const SDK_SYNC_TIMEOUT: Duration = Duration::from_secs(8);

pub struct HybridTodoBackend {
    data_dir: PathBuf,
    sdk_runtimes: Mutex<HashMap<String, Arc<SdkTodoRuntime>>>,
}

impl HybridTodoBackend {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            sdk_runtimes: Mutex::new(HashMap::new()),
        }
    }

    pub async fn refresh_thread_user_state(
        &self,
        thread_id: &str,
        user_id: Option<&str>,
        user_state: &mut Value,
    ) -> Result<()> {
        let runtime = self.sdk_runtime_for_user(user_id).await?;

        runtime
            .refresh_thread_user_state(thread_id, user_state)
            .await
    }

    pub async fn delete_thread(&self, thread_id: &str, user_id: Option<&str>) -> Result<()> {
        self.sdk_runtime_for_user(user_id)
            .await?
            .delete_thread(thread_id)
            .await
    }

    pub async fn fork_thread_user_state(
        &self,
        source_thread_id: &str,
        target_thread_id: &str,
        user_id: Option<&str>,
        target_user_state: &mut Value,
    ) -> Result<()> {
        self.sdk_runtime_for_user(user_id)
            .await?
            .fork_thread_user_state(source_thread_id, target_thread_id, target_user_state)
            .await
    }

    pub(crate) async fn add_batch(
        &self,
        ctx: &ToolContext,
        request: TodoBatchAddRequest,
    ) -> Result<TodoBatchAddResult, AgentError> {
        let thread_id = thread_id_from_ctx(ctx)
            .ok_or_else(|| AgentError::tool("todo__add", "missing thread_id in tool context"))?;
        let mut user_state = self.load_user_state(ctx).await;
        let todos = todos_from_user_state(&user_state);

        if should_create_via_sdk(ctx) {
            let runtime = self.require_sdk("todo__add", ctx).await?;
            let (mut updated, result) = add_batch_to_todos(todos, request);
            let new_todos = tag_new_sdk_batch(thread_id, &result, &mut updated);

            runtime
                .create_batch(thread_id, &result.batch_title, &new_todos)
                .await
                .map_err(|err| {
                    AgentError::tool(
                        "todo__add",
                        format!("failed to create sdk todo batch: {err:#}"),
                    )
                })?;

            write_todos_to_user_state(&mut user_state, &updated);
            store_user_state(ctx, user_state);
            return Ok(result);
        }

        let (updated, result) = add_batch_to_todos(todos, request);
        write_todos_to_user_state(&mut user_state, &updated);
        store_user_state(ctx, user_state);
        Ok(result)
    }

    pub async fn list(&self, ctx: &ToolContext) -> Vec<TodoItem> {
        let user_state = self.load_user_state(ctx).await;
        let todos = todos_from_user_state(&user_state);
        store_user_state(ctx, user_state);
        todos
    }

    pub async fn complete(&self, ctx: &ToolContext, id: u64) -> Result<String, AgentError> {
        let mut user_state = self.load_user_state(ctx).await;
        let mut todos = todos_from_user_state(&user_state);
        let Some(index) = todos.iter().position(|todo| todo.id == id) else {
            store_user_state(ctx, user_state);
            return Ok(format!("Todo #{id} not found."));
        };

        let todo = todos[index].clone();
        match todo.storage_kind {
            TodoStorageKind::Simple => {
                todos[index].done = true;
                write_todos_to_user_state(&mut user_state, &todos);
                store_user_state(ctx, user_state);
                Ok(format!("Todo #{id} marked as done."))
            }
            TodoStorageKind::RemiSdk => {
                let runtime = self.require_sdk("todo__complete", ctx).await?;
                let updated = runtime.complete_todo(&todo).await.map_err(|err| {
                    AgentError::tool(
                        "todo__complete",
                        format!("failed to update sdk todo status: {err:#}"),
                    )
                })?;

                if updated {
                    todos[index].done = true;
                    write_todos_to_user_state(&mut user_state, &todos);
                } else {
                    todos.remove(index);
                    write_todos_to_user_state(&mut user_state, &todos);
                }
                store_user_state(ctx, user_state);
                Ok(if updated {
                    format!("Todo #{id} marked as done.")
                } else {
                    format!("Todo #{id} not found.")
                })
            }
        }
    }

    pub async fn update(
        &self,
        ctx: &ToolContext,
        id: u64,
        content: String,
    ) -> Result<String, AgentError> {
        let mut user_state = self.load_user_state(ctx).await;
        let mut todos = todos_from_user_state(&user_state);
        let Some(index) = todos.iter().position(|todo| todo.id == id) else {
            store_user_state(ctx, user_state);
            return Ok(format!("Todo #{id} not found."));
        };

        let todo = todos[index].clone();
        match todo.storage_kind {
            TodoStorageKind::Simple => {
                todos[index].content = content.clone();
                write_todos_to_user_state(&mut user_state, &todos);
                store_user_state(ctx, user_state);
                Ok(format!("Updated todo #{id}: {content}"))
            }
            TodoStorageKind::RemiSdk => {
                let runtime = self.require_sdk("todo__update", ctx).await?;
                let updated = runtime
                    .update_todo_title(&todo, &content)
                    .await
                    .map_err(|err| {
                        AgentError::tool(
                            "todo__update",
                            format!("failed to update sdk todo title: {err:#}"),
                        )
                    })?;

                if updated {
                    todos[index].content = content.clone();
                } else {
                    todos.remove(index);
                }
                write_todos_to_user_state(&mut user_state, &todos);
                store_user_state(ctx, user_state);

                Ok(if updated {
                    format!("Updated todo #{id}: {content}")
                } else {
                    format!("Todo #{id} not found.")
                })
            }
        }
    }

    pub async fn remove(&self, ctx: &ToolContext, id: u64) -> Result<String, AgentError> {
        let mut user_state = self.load_user_state(ctx).await;
        let mut todos = todos_from_user_state(&user_state);
        let Some(index) = todos.iter().position(|todo| todo.id == id) else {
            store_user_state(ctx, user_state);
            return Ok(format!("Todo #{id} not found."));
        };

        let todo = todos[index].clone();
        match todo.storage_kind {
            TodoStorageKind::Simple => {
                todos.remove(index);
                write_todos_to_user_state(&mut user_state, &todos);
                store_user_state(ctx, user_state);
                Ok(format!("Removed todo #{id}."))
            }
            TodoStorageKind::RemiSdk => {
                let runtime = self.require_sdk("todo__remove", ctx).await?;
                let removed = runtime.remove_todo(&todo).await.map_err(|err| {
                    AgentError::tool(
                        "todo__remove",
                        format!("failed to remove sdk todo: {err:#}"),
                    )
                })?;
                todos.remove(index);
                write_todos_to_user_state(&mut user_state, &todos);
                store_user_state(ctx, user_state);
                Ok(if removed {
                    format!("Removed todo #{id}.")
                } else {
                    format!("Todo #{id} not found.")
                })
            }
        }
    }

    async fn require_sdk(
        &self,
        tool_name: &str,
        ctx: &ToolContext,
    ) -> Result<Arc<SdkTodoRuntime>, AgentError> {
        match self
            .sdk_runtime_for_user(metadata_string(ctx, "sender_user_id").as_deref())
            .await
        {
            Ok(runtime) => Ok(runtime),
            Err(err) => Err(AgentError::tool(
                tool_name,
                format!("remi sdk todo backend is unavailable: {err:#}"),
            )),
        }
    }

    async fn sdk_runtime_for_user(&self, user_id: Option<&str>) -> Result<Arc<SdkTodoRuntime>> {
        let user_key = sdk_user_key(user_id);
        {
            let runtimes = self.sdk_runtimes.lock().await;
            if let Some(runtime) = runtimes.get(&user_key) {
                return Ok(Arc::clone(runtime));
            }
        }

        let config = SdkTodoConfig::from_env(&self.data_dir, &user_key);
        let runtime = Arc::new(SdkTodoRuntime::initialize(config).await?);
        let mut runtimes = self.sdk_runtimes.lock().await;
        Ok(Arc::clone(
            runtimes
                .entry(user_key)
                .or_insert_with(|| Arc::clone(&runtime)),
        ))
    }

    async fn load_user_state(&self, ctx: &ToolContext) -> Value {
        let mut user_state = { ctx.user_state.read().unwrap().clone() };
        if let Some(thread_id) = thread_id_from_ctx(ctx) {
            if let Err(err) = self
                .refresh_thread_user_state(
                    thread_id,
                    metadata_string(ctx, "sender_user_id").as_deref(),
                    &mut user_state,
                )
                .await
            {
                warn!(
                    thread_id,
                    error = %err,
                    "failed to refresh sdk-backed todo state before tool execution"
                );
            }
        }
        user_state
    }
}

#[derive(Debug, Clone)]
struct SdkTodoConfig {
    remote: Option<SdkRemoteConfig>,
    device_id: String,
    db_path: PathBuf,
}

#[derive(Debug, Clone)]
struct SdkRemoteConfig {
    app_key: String,
    public_grpc_addr: String,
}

impl SdkTodoConfig {
    fn from_env(data_dir: &Path, user_key: &str) -> Self {
        let app_key = std::env::var(REMI_APP_KEY_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let public_grpc_addr = std::env::var(REMI_PUBLIC_GRPC_ADDR_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let remote = match (app_key, public_grpc_addr) {
            (Some(app_key), Some(public_grpc_addr)) => Some(SdkRemoteConfig {
                app_key,
                public_grpc_addr,
            }),
            (None, None) => None,
            _ => {
                warn!(
                    "partial remi sdk todo configuration detected; remote sync disabled until both {} and {} are set",
                    REMI_APP_KEY_ENV,
                    REMI_PUBLIC_GRPC_ADDR_ENV,
                );
                None
            }
        };
        Self {
            remote,
            device_id: std::env::var(REMI_TODO_DEVICE_ID_ENV)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| default_device_id(data_dir, user_key)),
            db_path: sdk_user_dir(data_dir, user_key).join(SDK_DB_FILE_NAME),
        }
    }
}

struct SdkTodoRuntime {
    config: SdkTodoConfig,
    sdk: Arc<TriggerSdk>,
    lock: Arc<Mutex<()>>,
    lock_owner: Arc<StdMutex<Option<String>>>,
}

impl SdkTodoRuntime {
    async fn initialize(config: SdkTodoConfig) -> Result<Self> {
        if let Some(parent) = config.db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating sdk todo dir {}", parent.display()))?;
        }
        if let Some(remote) = &config.remote {
            let transport_config = serde_json::json!({
                "transportMode": "tcp",
                "tcpGrpcAddr": remote.public_grpc_addr,
                "connectTimeoutMs": 3000,
                "requestTimeoutMs": 10000,
            })
            .to_string();

            configure_shared_transport(&transport_config)
                .await
                .map_err(|err| anyhow::anyhow!(err))?;
            auth_set_app_key(remote.app_key.clone())
                .await
                .map_err(|err| anyhow::anyhow!(err))?;
        }

        let sdk = Arc::new(TriggerSdk::initialize(&config.db_path).with_context(|| {
            format!(
                "failed to initialize sdk todo database at {}",
                config.db_path.display()
            )
        })?);
        let lock = Arc::new(Mutex::new(()));
        let lock_owner = Arc::new(StdMutex::new(None));
        if config.remote.is_some() {
            spawn_things_sync_task(Arc::clone(&sdk), config.clone());
        }

        Ok(Self {
            sdk,
            config,
            lock,
            lock_owner,
        })
    }

    async fn refresh_thread_user_state(
        &self,
        thread_id: &str,
        user_state: &mut Value,
    ) -> Result<()> {
        let sdk_todos = {
            let _guard = self.lock_for("refresh_thread_user_state").await;
            self.thread_todos_from_snapshot_locked(thread_id)?
        };
        merge_sdk_todos(user_state, sdk_todos);
        Ok(())
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<()> {
        let _guard = self.lock_for("delete_thread").await;
        let todos = self.thread_todos_from_snapshot_locked(thread_id)?;
        let collections: HashSet<String> = todos
            .iter()
            .filter_map(|todo| todo.collection_uuid.clone())
            .collect();
        for todo in todos {
            let (Some(collection_uuid), Some(thing_uuid)) =
                (todo.collection_uuid.as_deref(), todo.thing_uuid.as_deref())
            else {
                continue;
            };
            let _ = self.sdk.things_delete_thing(
                &self.config.device_id,
                collection_uuid,
                thing_uuid,
            )?;
        }
        for collection_uuid in collections {
            let _ = self
                .sdk
                .things_delete_collection(&self.config.device_id, &collection_uuid)?;
        }
        Ok(())
    }

    async fn fork_thread_user_state(
        &self,
        source_thread_id: &str,
        target_thread_id: &str,
        target_user_state: &mut Value,
    ) -> Result<()> {
        let _guard = self.lock_for("fork_thread_user_state").await;
        let source_todos = self.thread_todos_from_snapshot_locked(source_thread_id)?;
        let mut copied = Vec::with_capacity(source_todos.len());
        let mut by_batch: HashMap<u64, Vec<TodoItem>> = HashMap::new();
        for mut todo in source_todos {
            let batch_id = todo.batch_id.unwrap_or(todo.id);
            todo.storage_kind = TodoStorageKind::RemiSdk;
            todo.collection_uuid = Some(collection_uuid_for(target_thread_id, batch_id));
            todo.thing_uuid = Some(thing_uuid_for(target_thread_id, todo.id));
            by_batch.entry(batch_id).or_default().push(todo.clone());
            copied.push(todo);
        }
        for (_, mut todos) in by_batch {
            todos.sort_by_key(|todo| todo.batch_index.unwrap_or(todo.id));
            let batch_title = todos
                .first()
                .and_then(|todo| todo.batch_title.as_deref())
                .unwrap_or("Forked todos");
            self.create_batch_locked(target_thread_id, batch_title, &todos)?;
            for todo in todos.iter().filter(|todo| todo.done) {
                let _ = self.complete_todo_locked(todo)?;
            }
        }
        merge_sdk_todos(target_user_state, copied);
        Ok(())
    }

    async fn create_batch(
        &self,
        thread_id: &str,
        batch_title: &str,
        todos: &[TodoItem],
    ) -> Result<()> {
        let _guard = self.lock_for("create_batch").await;
        self.create_batch_locked(thread_id, batch_title, todos)
    }

    fn create_batch_locked(
        &self,
        thread_id: &str,
        batch_title: &str,
        todos: &[TodoItem],
    ) -> Result<()> {
        let Some(first) = todos.first() else {
            return Ok(());
        };
        let collection_uuid = first
            .collection_uuid
            .as_deref()
            .context("missing collection uuid for sdk todo batch")?;

        let collection = ThingCollectionUpsert {
            uuid: collection_uuid.to_string(),
            title: batch_title.to_string(),
            trigger_uuid: None,
            created_at: None,
            updated_at: None,
        };
        self.sdk
            .things_upsert_collection(&self.config.device_id, collection)?;

        for todo in todos {
            let thing_uuid = todo
                .thing_uuid
                .as_deref()
                .context("missing thing uuid for sdk todo item")?;
            let payload = ThingUpsert {
                uuid: thing_uuid.to_string(),
                title: todo.content.clone(),
                datatype: ThingDatatype::Todo,
                data: None,
                collection_uuid: collection_uuid.to_string(),
                trigger_uuid: None,
                parent_uuid: None,
                created_at: None,
                updated_at: None,
            };
            self.sdk
                .things_upsert_thing(&self.config.device_id, payload)?;
            self.patch_thing_attrs_locked(
                collection_uuid,
                thing_uuid,
                todo_attrs_json(thread_id, todo),
            )?;
        }
        Ok(())
    }

    async fn complete_todo(&self, todo: &TodoItem) -> Result<bool> {
        let _guard = self.lock_for("complete_todo").await;
        self.complete_todo_locked(todo)
    }

    fn complete_todo_locked(&self, todo: &TodoItem) -> Result<bool> {
        let thing_uuid = todo
            .thing_uuid
            .as_deref()
            .context("missing thing uuid for sdk todo item")?;
        let updated = self
            .sdk
            .set_thing_status(&self.config.device_id, thing_uuid, "done")?;
        Ok(updated)
    }

    async fn update_todo_title(&self, todo: &TodoItem, content: &str) -> Result<bool> {
        let _guard = self.lock_for("update_todo_title").await;
        let thing_uuid = todo
            .thing_uuid
            .as_deref()
            .context("missing thing uuid for sdk todo item")?;
        let result = self.sdk.things_edit_content(
            &self.config.device_id,
            thing_uuid,
            "set_title",
            Some(content),
            None,
            None,
            None,
            None,
            None,
            None,
        )?;
        let json: Value = serde_json::from_str(&result).unwrap_or(Value::Null);
        let found = json
            .get("error")
            .and_then(Value::as_str)
            .map(|value| value != "thing_not_found")
            .unwrap_or(true);
        Ok(found)
    }

    async fn remove_todo(&self, todo: &TodoItem) -> Result<bool> {
        let _guard = self.lock_for("remove_todo").await;
        let collection_uuid = todo
            .collection_uuid
            .as_deref()
            .context("missing collection uuid for sdk todo item")?;
        let thing_uuid = todo
            .thing_uuid
            .as_deref()
            .context("missing thing uuid for sdk todo item")?;

        let removed =
            self.sdk
                .things_delete_thing(&self.config.device_id, collection_uuid, thing_uuid)?;
        if removed {
            let snapshot = self.snapshot_locked()?;
            let has_remaining_items = snapshot
                .things
                .iter()
                .any(|thing| thing.collection_uuid == collection_uuid);
            if !has_remaining_items {
                let _ = self
                    .sdk
                    .things_delete_collection(&self.config.device_id, collection_uuid);
            }
        }
        Ok(removed)
    }

    async fn lock_for(&self, operation: &'static str) -> SdkTodoLockGuard<'_> {
        lock_sdk_todo_mutex(&self.lock, &self.lock_owner, operation).await
    }

    fn snapshot_locked(&self) -> Result<ThingsSnapshotState> {
        self.sdk
            .things_list_snapshot_lite(&self.config.device_id)
            .context("failed to read local sdk todo snapshot")
    }

    fn thread_todos_from_snapshot_locked(&self, thread_id: &str) -> Result<Vec<TodoItem>> {
        let snapshot = self.snapshot_locked()?;
        Ok(thread_todos_from_snapshot(thread_id, &snapshot))
    }

    fn patch_thing_attrs_locked(
        &self,
        collection_uuid: &str,
        thing_uuid: &str,
        attrs_json: Value,
    ) -> Result<()> {
        let row = self
            .sdk
            .crdt_get_document(collection_uuid, "collection")?
            .ok_or_else(|| anyhow::anyhow!("collection document not found: {collection_uuid}"))?;
        let updated_doc = apply_collection_op(
            &row.automerge_doc,
            &self.config.device_id,
            collection_uuid,
            CollectionOp::UpsertThingMeta {
                thing_id: thing_uuid.to_string(),
                datatype: None,
                status: None,
                status_timestamp_ms: None,
                title: None,
                parent_id: None,
                trigger: TriggerUpdate::Noop,
                built_in: None,
                attrs_json: Some(serde_json::to_string(&attrs_json)?),
            },
        )?;
        self.sdk.crdt_save_document(
            collection_uuid,
            "collection",
            &updated_doc,
            &row.sync_state,
            true,
            row.last_sync_at.as_deref(),
        )?;
        Ok(())
    }
}

struct SdkTodoLockGuard<'a> {
    _guard: MutexGuard<'a, ()>,
    owner: Arc<StdMutex<Option<String>>>,
}

impl Drop for SdkTodoLockGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut owner) = self.owner.lock() {
            *owner = None;
        }
    }
}

async fn lock_sdk_todo_mutex<'a>(
    lock: &'a Arc<Mutex<()>>,
    owner: &Arc<StdMutex<Option<String>>>,
    operation: &'static str,
) -> SdkTodoLockGuard<'a> {
    let owner_label = operation.to_string();
    match lock.try_lock() {
        Ok(guard) => {
            if let Ok(mut owner) = owner.lock() {
                *owner = Some(owner_label);
            }
            info!(operation, waited_ms = 0_u64, "sdk todo lock acquired");
            return SdkTodoLockGuard {
                _guard: guard,
                owner: Arc::clone(owner),
            };
        }
        Err(_) => {
            let current_owner = owner
                .lock()
                .ok()
                .and_then(|owner| owner.clone())
                .unwrap_or_else(|| "unknown".to_string());
            warn!(operation, current_owner, "sdk todo lock waiting");
        }
    }

    let started = Instant::now();
    let guard = lock.lock().await;
    let waited = started.elapsed();
    if let Ok(mut owner) = owner.lock() {
        *owner = Some(owner_label);
    }
    if waited >= SDK_LOCK_WARN_AFTER {
        warn!(
            operation,
            waited_ms = waited.as_millis() as u64,
            "sdk todo lock waited"
        );
    } else {
        info!(
            operation,
            waited_ms = waited.as_millis() as u64,
            "sdk todo lock acquired"
        );
    }
    SdkTodoLockGuard {
        _guard: guard,
        owner: Arc::clone(owner),
    }
}

fn spawn_things_sync_task(sdk: Arc<TriggerSdk>, config: SdkTodoConfig) {
    tokio::spawn(async move {
        let mut rx = sdk.things_subscribe();
        loop {
            let should_sync = match rx.recv().await {
                Ok(event) => should_sync_on_things_event(&event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(
                        device_id = %config.device_id,
                        skipped,
                        "sdk todo things event subscriber lagged; forcing sync"
                    );
                    true
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };

            if !should_sync {
                continue;
            }

            while let Ok(event) = rx.try_recv() {
                if should_sync_on_things_event(&event) {
                    continue;
                }
            }

            info!(
                device_id = %config.device_id,
                timeout_ms = SDK_SYNC_TIMEOUT.as_millis() as u64,
                mode = ?ThingsSyncMode::Incremental,
                "sdk todo background sync started"
            );
            match timeout(SDK_SYNC_TIMEOUT, sync_sdk_todos(&sdk, &config)).await {
                Ok(Ok(output)) => {
                    info!(
                        device_id = %config.device_id,
                        documents_synced = output.documents_synced,
                        total_elapsed_ms = output.metrics.total_elapsed_ms,
                        phase1_push_ms = output.metrics.phase1_push_ms,
                        phase1_documents_synced = output.metrics.phase1_documents_synced,
                        phase1_batch_calls = output.metrics.phase1_batch_calls,
                        phase1_rpc_rounds = output.metrics.phase1_rpc_rounds,
                        "sdk todo background sync completed"
                    );
                }
                Ok(Err(err)) => {
                    warn!(
                        device_id = %config.device_id,
                        error = %err,
                        "sdk todo background sync triggered by things event failed"
                    );
                }
                Err(_) => {
                    warn!(
                        device_id = %config.device_id,
                        timeout_ms = SDK_SYNC_TIMEOUT.as_millis() as u64,
                        mode = ?ThingsSyncMode::Incremental,
                        "sdk todo background sync triggered by things event timed out"
                    );
                }
            }
        }
    });
}

fn should_sync_on_things_event(event: &ThingsEvent) -> bool {
    matches!(
        event,
        ThingsEvent::SnapshotReplaced { .. }
            | ThingsEvent::DocumentChanged { .. }
            | ThingsEvent::DataWiped
    )
}

async fn sync_sdk_todos(
    sdk: &TriggerSdk,
    config: &SdkTodoConfig,
) -> Result<remi_client_sdk::things_sync::ThingsV3SyncOutput> {
    let mut client = TriggerClient::new_with_shared_transport(String::new()).await?;
    sync_v3_documents_with_server_mode(
        sdk,
        &mut client,
        &config.device_id,
        ThingsSyncMode::Incremental,
    )
    .await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSdkTodoAttrs {
    source: String,
    thread_id: String,
    local_id: u64,
    batch_id: u64,
    batch_index: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

fn store_user_state(ctx: &ToolContext, user_state: Value) {
    *ctx.user_state.write().unwrap() = user_state;
}

fn should_create_via_sdk(ctx: &ToolContext) -> bool {
    ctx.metadata
        .as_ref()
        .and_then(|value| value.get(TODO_CREATE_VIA_SDK_META_KEY))
        .is_some_and(metadata_flag_enabled)
}

fn metadata_flag_enabled(value: &Value) -> bool {
    value.as_bool().unwrap_or_else(|| {
        value
            .as_str()
            .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True"))
            .unwrap_or(false)
    })
}

fn metadata_string(ctx: &ToolContext, key: &str) -> Option<String> {
    ctx.metadata
        .as_ref()
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn thread_id_from_ctx<'a>(ctx: &'a ToolContext) -> Option<&'a str> {
    ctx.thread_id
        .as_ref()
        .map(|thread_id| thread_id.0.as_str())
        .or_else(|| {
            ctx.metadata
                .as_ref()
                .and_then(|value| value.get("thread_id"))
                .and_then(Value::as_str)
        })
}

fn default_device_id(data_dir: &Path, user_key: &str) -> String {
    let stable_path = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    format!(
        "remi-cat-{}",
        Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("{}:{user_key}", stable_path.to_string_lossy()).as_bytes()
        )
    )
}

fn sdk_user_dir(data_dir: &Path, user_key: &str) -> PathBuf {
    data_dir.join("sdk").join(user_key).join("todo")
}

fn sdk_user_key(user_id: Option<&str>) -> String {
    let raw = user_id.unwrap_or("anonymous").trim();
    let raw = if raw.is_empty() { "anonymous" } else { raw };
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn tag_new_sdk_batch(
    thread_id: &str,
    result: &TodoBatchAddResult,
    todos: &mut [TodoItem],
) -> Vec<TodoItem> {
    let todo_ids: HashSet<u64> = result.todo_ids.iter().copied().collect();
    let collection_uuid = collection_uuid_for(thread_id, result.batch_id);
    let mut tagged = Vec::with_capacity(result.todo_ids.len());
    for todo in todos.iter_mut() {
        if !todo_ids.contains(&todo.id) {
            continue;
        }
        todo.storage_kind = TodoStorageKind::RemiSdk;
        todo.collection_uuid = Some(collection_uuid.clone());
        todo.thing_uuid = Some(thing_uuid_for(thread_id, todo.id));
        tagged.push(todo.clone());
    }
    tagged.sort_by_key(|todo| todo.id);
    tagged
}

fn merge_sdk_todos(user_state: &mut Value, sdk_todos: Vec<TodoItem>) {
    let mut merged: Vec<TodoItem> = todos_from_user_state(user_state)
        .into_iter()
        .filter(|todo| todo.storage_kind != TodoStorageKind::RemiSdk)
        .collect();
    merged.extend(sdk_todos);
    merged.sort_by_key(|todo| todo.id);
    write_todos_to_user_state(user_state, &merged);
}

fn thread_todos_from_snapshot(thread_id: &str, snapshot: &ThingsSnapshotState) -> Vec<TodoItem> {
    let collection_titles: HashMap<&str, &str> = snapshot
        .collections
        .iter()
        .map(|collection| (collection.uuid.as_str(), collection.title.as_str()))
        .collect();

    let mut todos = Vec::new();
    for thing in &snapshot.things {
        let Some(attrs) = stored_attrs_from_thing(thing) else {
            continue;
        };
        if attrs.source != SDK_ATTRS_SOURCE || attrs.thread_id != thread_id {
            continue;
        }
        let batch_title = collection_titles
            .get(thing.collection_uuid.as_str())
            .map(|value| (*value).to_string())
            .filter(|value| !value.trim().is_empty());
        todos.push(TodoItem {
            id: attrs.local_id,
            content: thing.title.clone(),
            description: attrs.description.clone(),
            done: thing.status == "done",
            batch_id: Some(attrs.batch_id),
            batch_title,
            batch_index: Some(attrs.batch_index),
            storage_kind: TodoStorageKind::RemiSdk,
            collection_uuid: Some(thing.collection_uuid.clone()),
            thing_uuid: Some(thing.uuid.clone()),
        });
    }
    todos.sort_by_key(|todo| {
        (
            todo.batch_id.unwrap_or(u64::MAX),
            todo.batch_index.unwrap_or(u64::MAX),
            todo.id,
        )
    });
    todos
}

fn stored_attrs_from_thing(
    thing: &remi_client_sdk::things_crdt::ThingEntry,
) -> Option<StoredSdkTodoAttrs> {
    let attrs = thing.data.get("attrs")?;
    let attrs = attrs.get(SDK_ATTRS_ROOT_KEY)?;
    serde_json::from_value(attrs.clone()).ok()
}

fn todo_attrs_json(thread_id: &str, todo: &TodoItem) -> Value {
    let attrs = StoredSdkTodoAttrs {
        source: SDK_ATTRS_SOURCE.to_string(),
        thread_id: thread_id.to_string(),
        local_id: todo.id,
        batch_id: todo.batch_id.unwrap_or(todo.id),
        batch_index: todo.batch_index.unwrap_or(0),
        description: todo.description.clone(),
    };
    serde_json::json!({ SDK_ATTRS_ROOT_KEY: attrs })
}

fn collection_uuid_for(thread_id: &str, batch_id: u64) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("remi-cat/todo/batch/{thread_id}/{batch_id}").as_bytes(),
    )
    .to_string()
}

fn thing_uuid_for(thread_id: &str, todo_id: u64) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("remi-cat/todo/item/{thread_id}/{todo_id}").as_bytes(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        merge_sdk_todos, thread_todos_from_snapshot, SDK_ATTRS_ROOT_KEY, SDK_ATTRS_SOURCE,
    };
    use crate::todo::tools::{TodoItem, TodoStorageKind};
    use remi_client_sdk::things_crdt::{ThingCollectionEntry, ThingEntry, ThingsSnapshotState};
    use remi_things_crdt::ThingDatatype;
    use serde_json::json;

    fn sdk_attrs(
        thread_id: &str,
        local_id: u64,
        batch_id: u64,
        batch_index: u64,
    ) -> serde_json::Value {
        json!({
            SDK_ATTRS_ROOT_KEY: {
                "source": SDK_ATTRS_SOURCE,
                "thread_id": thread_id,
                "local_id": local_id,
                "batch_id": batch_id,
                "batch_index": batch_index,
                "description": "from sdk"
            }
        })
    }

    #[test]
    fn snapshot_filter_only_keeps_matching_thread_items() {
        let snapshot = ThingsSnapshotState {
            collections: vec![ThingCollectionEntry {
                uuid: "collection-1".to_string(),
                title: "Release launch".to_string(),
                trigger_uuid: None,
                created_at: String::new(),
                updated_at: String::new(),
                actor_type: None,
                actor_app_id: None,
                actor_display_name: None,
                card_jsx: None,
            }],
            things: vec![
                ThingEntry {
                    uuid: "thing-1".to_string(),
                    title: "Draft changelog".to_string(),
                    datatype: ThingDatatype::Todo,
                    data: json!({ "attrs": sdk_attrs("thread-a", 7, 7, 0) }),
                    collection_uuid: "collection-1".to_string(),
                    trigger_uuid: None,
                    parent_uuid: None,
                    created_at: String::new(),
                    updated_at: String::new(),
                    status: "done".to_string(),
                    status_timestamp_ms: None,
                    actor_type: None,
                    actor_app_id: None,
                    actor_display_name: None,
                },
                ThingEntry {
                    uuid: "thing-2".to_string(),
                    title: "Other thread".to_string(),
                    datatype: ThingDatatype::Todo,
                    data: json!({ "attrs": sdk_attrs("thread-b", 8, 8, 0) }),
                    collection_uuid: "collection-1".to_string(),
                    trigger_uuid: None,
                    parent_uuid: None,
                    created_at: String::new(),
                    updated_at: String::new(),
                    status: "none".to_string(),
                    status_timestamp_ms: None,
                    actor_type: None,
                    actor_app_id: None,
                    actor_display_name: None,
                },
            ],
            dirty: false,
            last_sync_at: None,
        };

        let todos = thread_todos_from_snapshot("thread-a", &snapshot);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].id, 7);
        assert!(todos[0].done);
        assert_eq!(todos[0].batch_title.as_deref(), Some("Release launch"));
    }

    #[test]
    fn merge_replaces_only_sdk_items() {
        let mut user_state = json!({
            "__todos": [
                TodoItem {
                    id: 1,
                    content: "simple".to_string(),
                    description: None,
                    done: false,
                    batch_id: None,
                    batch_title: None,
                    batch_index: None,
                    storage_kind: TodoStorageKind::Simple,
                    collection_uuid: None,
                    thing_uuid: None,
                },
                TodoItem {
                    id: 2,
                    content: "old sdk".to_string(),
                    description: None,
                    done: false,
                    batch_id: Some(2),
                    batch_title: Some("old".to_string()),
                    batch_index: Some(0),
                    storage_kind: TodoStorageKind::RemiSdk,
                    collection_uuid: Some("c-old".to_string()),
                    thing_uuid: Some("t-old".to_string()),
                }
            ]
        });

        merge_sdk_todos(
            &mut user_state,
            vec![TodoItem {
                id: 3,
                content: "new sdk".to_string(),
                description: None,
                done: false,
                batch_id: Some(3),
                batch_title: Some("new".to_string()),
                batch_index: Some(0),
                storage_kind: TodoStorageKind::RemiSdk,
                collection_uuid: Some("c-new".to_string()),
                thing_uuid: Some("t-new".to_string()),
            }],
        );

        let todos: Vec<TodoItem> = serde_json::from_value(user_state["__todos"].clone())
            .expect("todos should deserialize");
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].id, 1);
        assert_eq!(todos[1].id, 3);
    }
}
