use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_stream::stream;
use bot_runtime_core::ToolContext;
use chrono::Utc;
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    AgentError, CancellationToken, ResumePayload, Tool, ToolOutput, ToolResult,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, Mutex};

use crate::CatEvent;

const STORE_DIR: &str = "tool_tasks";
const LEGACY_STORE_FILE: &str = "tool_tasks.json";
const MAX_COMPLETED_TASKS: usize = 100;
pub const TOOL_TASK_RUNNING: &str = "running";
pub const TOOL_TASK_COMPLETED: &str = "completed";
pub const TOOL_TASK_FAILED: &str = "failed";
pub const TOOL_TASK_CANCELLED: &str = "cancelled";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolTaskRecord {
    pub task_id: String,
    pub thread_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub status: String,
    #[serde(default = "default_true")]
    pub background: bool,
    pub started_at: String,
    #[serde(default)]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub result_preview: Option<String>,
    #[serde(default)]
    pub recent_output: Vec<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub notify_on_finish: bool,
    #[serde(default)]
    pub notification_delivered: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ToolTaskStore {
    tasks: HashMap<String, ToolTaskRecord>,
}

#[derive(Debug)]
struct RunningTask {
    cancel: CancellationToken,
    abort: Option<tokio::task::AbortHandle>,
}

#[derive(Debug)]
struct PerThreadManager {
    thread_id: String,
    path: PathBuf,
    store: Mutex<ToolTaskStore>,
    persist_lock: Mutex<()>,
    running: Mutex<HashMap<String, RunningTask>>,
    completed_tx: broadcast::Sender<ToolTaskRecord>,
    side_event_tx: broadcast::Sender<(String, CatEvent)>,
}

impl PerThreadManager {
    async fn start_inner(
        &self,
        run_id: String,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        cancel: CancellationToken,
        app_id: Option<String>,
    ) -> String {
        let task_id = uuid::Uuid::new_v4().to_string();
        let record = ToolTaskRecord {
            task_id: task_id.clone(),
            thread_id: self.thread_id.clone(),
            run_id,
            app_id,
            tool_call_id,
            tool_name,
            args,
            status: TOOL_TASK_RUNNING.to_string(),
            background: false,
            started_at: Utc::now().to_rfc3339(),
            completed_at: None,
            elapsed_ms: None,
            success: None,
            result_preview: None,
            recent_output: Vec::new(),
            message: None,
            notify_on_finish: false,
            notification_delivered: false,
        };
        self.running.lock().await.insert(
            task_id.clone(),
            RunningTask {
                cancel,
                abort: None,
            },
        );
        self.store
            .lock()
            .await
            .tasks
            .insert(task_id.clone(), record);
        let _ = self.save().await;
        task_id
    }

    async fn attach_abort_handle(&self, task_id: &str, abort: tokio::task::AbortHandle) {
        if let Some(running) = self.running.lock().await.get_mut(task_id) {
            running.abort = Some(abort);
        } else {
            abort.abort();
        }
    }

    async fn enable_completion_notification(&self, task_id: &str) -> Option<ToolTaskRecord> {
        let mut store = self.store.lock().await;
        let record = store.tasks.get_mut(task_id)?;
        record.notify_on_finish = true;
        let snapshot = record.clone();
        drop(store);
        if snapshot.status == TOOL_TASK_RUNNING {
            return None;
        }
        if !snapshot.notification_delivered {
            let _ = self.completed_tx.send(snapshot.clone());
        }
        Some(snapshot)
    }

    async fn claim_completion_notification(&self, task_id: &str) -> bool {
        let mut store = self.store.lock().await;
        let Some(record) = store.tasks.get_mut(task_id) else {
            return false;
        };
        if record.status == TOOL_TASK_RUNNING
            || !record.notify_on_finish
            || record.notification_delivered
        {
            return false;
        }
        record.notification_delivered = true;
        true
    }

    async fn claim_pending_completion_notification(&self) -> Option<ToolTaskRecord> {
        let mut store = self.store.lock().await;
        for record in store.tasks.values_mut() {
            if record.status != TOOL_TASK_RUNNING
                && record.notify_on_finish
                && !record.notification_delivered
            {
                record.notification_delivered = true;
                return Some(record.clone());
            }
        }
        None
    }

    async fn append_output(&self, task_id: &str, line: impl Into<String>) {
        let mut store = self.store.lock().await;
        if let Some(record) = store.tasks.get_mut(task_id) {
            record.recent_output.push(line.into());
            if record.recent_output.len() > 20 {
                let drop_count = record.recent_output.len() - 20;
                record.recent_output.drain(0..drop_count);
            }
        }
    }

    async fn finish(
        &self,
        task_id: &str,
        success: bool,
        elapsed_ms: u64,
        result_preview: String,
    ) -> Option<ToolTaskRecord> {
        self.running.lock().await.remove(task_id);
        let mut store = self.store.lock().await;
        let record = store.tasks.get_mut(task_id)?;
        if record.status != TOOL_TASK_RUNNING {
            return Some(record.clone());
        }
        record.status = if success {
            TOOL_TASK_COMPLETED
        } else {
            TOOL_TASK_FAILED
        }
        .to_string();
        record.completed_at = Some(Utc::now().to_rfc3339());
        record.elapsed_ms = Some(elapsed_ms);
        record.success = Some(success);
        record.result_preview = Some(result_preview);
        let notify = record.notify_on_finish && !record.notification_delivered;
        let cloned = record.clone();
        drop(store);
        let _ = self.save().await;
        if notify {
            let _ = self.completed_tx.send(cloned.clone());
        }
        Some(cloned)
    }

    async fn cancel(&self, task_id: &str) -> Option<ToolTaskRecord> {
        if let Some(running) = self.running.lock().await.remove(task_id) {
            running.cancel.cancel();
            if let Some(abort) = running.abort {
                abort.abort();
            }
        }
        let mut store = self.store.lock().await;
        let record = store.tasks.get_mut(task_id)?;
        let mut changed = false;
        if record.status == TOOL_TASK_RUNNING {
            let started_at = chrono::DateTime::parse_from_rfc3339(&record.started_at).ok();
            record.status = TOOL_TASK_CANCELLED.to_string();
            let completed_at = Utc::now();
            record.completed_at = Some(completed_at.to_rfc3339());
            record.elapsed_ms = started_at.map(|started_at| {
                completed_at
                    .signed_duration_since(started_at.with_timezone(&Utc))
                    .num_milliseconds()
                    .max(0) as u64
            });
            record.success = Some(false);
            record.message = Some("cancelled".to_string());
            changed = true;
        }
        let cloned = record.clone();
        drop(store);
        if changed {
            let _ = self.save().await;
        }
        Some(cloned)
    }

    async fn cancel_all_running(&self) -> Vec<ToolTaskRecord> {
        let ids: Vec<String> = {
            let store = self.store.lock().await;
            store
                .tasks
                .values()
                .filter(|t| t.status == TOOL_TASK_RUNNING)
                .map(|t| t.task_id.clone())
                .collect()
        };
        let mut cancelled = Vec::new();
        for id in ids {
            if let Some(task) = self.cancel(&id).await {
                cancelled.push(task);
            }
        }
        cancelled
    }

    async fn list(&self) -> Vec<ToolTaskRecord> {
        let mut records: Vec<ToolTaskRecord> = self
            .store
            .lock()
            .await
            .tasks
            .values()
            .filter(|task| task.background)
            .cloned()
            .collect();
        records.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        records
    }

    async fn get(&self, task_id: &str) -> Option<ToolTaskRecord> {
        self.store.lock().await.tasks.get(task_id).cloned()
    }

    async fn promote_to_background(&self, task_id: &str, notify: bool) -> Option<ToolTaskRecord> {
        let mut store = self.store.lock().await;
        let record = store.tasks.get_mut(task_id)?;
        record.background = true;
        if notify {
            record.notify_on_finish = true;
        }
        let snapshot = record.clone();
        drop(store);
        let _ = self.save().await;
        if snapshot.status != TOOL_TASK_RUNNING
            && snapshot.notify_on_finish
            && !snapshot.notification_delivered
        {
            let _ = self.completed_tx.send(snapshot.clone());
        }
        Some(snapshot)
    }

    async fn remove_foreground(&self, task_id: &str) {
        self.running.lock().await.remove(task_id);
        let removed = self.store.lock().await.tasks.remove(task_id).is_some();
        if removed {
            let _ = self.save().await;
        }
    }

    async fn is_thread_running(&self) -> bool {
        self.store
            .lock()
            .await
            .tasks
            .values()
            .any(|t| t.status == TOOL_TASK_RUNNING)
    }

    async fn save(&self) -> Result<()> {
        let _guard = self.persist_lock.lock().await;
        let mut store = self.store.lock().await;
        prune_completed_tasks(&mut store);
        let raw = serde_json::to_string_pretty(&*store)?;
        let path = self.path.clone();
        drop(store);
        tokio::task::spawn_blocking(move || save_store_raw(&path, raw))
            .await
            .map_err(|err| anyhow::anyhow!("tool task store writer failed: {err}"))?
    }
}

#[derive(Debug)]
pub struct ToolTaskManager {
    data_dir: PathBuf,
    threads: Mutex<HashMap<String, Arc<PerThreadManager>>>,
    /// Maps task_id -> thread_id for cross-thread lookups (cancel, get, etc.)
    task_thread_map: Mutex<HashMap<String, String>>,
}

impl ToolTaskManager {
    pub fn load(data_dir: impl AsRef<Path>) -> Result<Arc<Self>> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let store_dir = data_dir.join(STORE_DIR);
        std::fs::create_dir_all(&store_dir).with_context(|| "creating tool task dir")?;
        let legacy = data_dir.join(LEGACY_STORE_FILE);
        let mut source_paths = Vec::new();
        if legacy.exists() {
            source_paths.push(legacy);
        }
        for entry in std::fs::read_dir(&store_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                source_paths.push(path);
            }
        }

        let mut grouped: HashMap<String, ToolTaskStore> = HashMap::new();
        for path in &source_paths {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let store = serde_json::from_str::<ToolTaskStore>(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            let fallback_thread_id = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string();
            for (task_id, mut task) in store.tasks {
                if task.thread_id.is_empty() {
                    task.thread_id = fallback_thread_id.clone();
                }
                grouped
                    .entry(task.thread_id.clone())
                    .or_default()
                    .tasks
                    .insert(task_id, task);
            }
        }

        let mut threads = HashMap::new();
        let mut task_thread_map = HashMap::new();
        let mut destination_paths = std::collections::HashSet::new();
        for (thread_id, mut store) in grouped {
            normalize_loaded_store(&mut store);
            let path = thread_store_path(&data_dir, &thread_id);
            save_store(&path, &store)?;
            destination_paths.insert(path.clone());
            for task_id in store.tasks.keys() {
                task_thread_map.insert(task_id.clone(), thread_id.clone());
            }
            threads.insert(
                thread_id.clone(),
                Arc::new(PerThreadManager {
                    thread_id,
                    path,
                    store: Mutex::new(store),
                    persist_lock: Mutex::new(()),
                    running: Mutex::new(HashMap::new()),
                    completed_tx: broadcast::channel(64).0,
                    side_event_tx: broadcast::channel(256).0,
                }),
            );
        }
        for path in source_paths {
            if !destination_paths.contains(&path) {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(Arc::new(Self {
            data_dir,
            threads: Mutex::new(threads),
            task_thread_map: Mutex::new(task_thread_map),
        }))
    }

    fn load_per_thread(data_dir: &Path, thread_id: &str) -> Result<Arc<PerThreadManager>> {
        let path = thread_store_path(data_dir, thread_id);
        let mut store = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<ToolTaskStore>(&raw)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ToolTaskStore::default(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        normalize_loaded_store(&mut store);
        save_store(&path, &store)?;
        Ok(Arc::new(PerThreadManager {
            thread_id: thread_id.to_string(),
            path,
            store: Mutex::new(store),
            persist_lock: Mutex::new(()),
            running: Mutex::new(HashMap::new()),
            completed_tx: broadcast::channel(64).0,
            side_event_tx: broadcast::channel(256).0,
        }))
    }

    async fn get_thread(&self, thread_id: &str) -> Result<Arc<PerThreadManager>> {
        {
            let threads = self.threads.lock().await;
            if let Some(mgr) = threads.get(thread_id) {
                return Ok(Arc::clone(mgr));
            }
        }
        let mgr = Self::load_per_thread(&self.data_dir, thread_id)?;
        let mut threads = self.threads.lock().await;
        // Double-check after acquiring lock
        if let Some(existing) = threads.get(thread_id) {
            return Ok(Arc::clone(existing));
        }
        threads.insert(thread_id.to_string(), Arc::clone(&mgr));
        Ok(mgr)
    }

    pub async fn start(
        &self,
        thread_id: String,
        run_id: String,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<String> {
        let mgr = self.get_thread(&thread_id).await?;
        let task_id = mgr
            .start_inner(run_id, tool_call_id, tool_name, args, cancel, None)
            .await;
        self.task_thread_map
            .lock()
            .await
            .insert(task_id.clone(), thread_id);
        Ok(task_id)
    }

    pub async fn start_with_app_id(
        &self,
        thread_id: String,
        run_id: String,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        cancel: CancellationToken,
        app_id: Option<String>,
    ) -> Result<String> {
        let mgr = self.get_thread(&thread_id).await?;
        let task_id = mgr
            .start_inner(run_id, tool_call_id, tool_name, args, cancel, app_id)
            .await;
        self.task_thread_map
            .lock()
            .await
            .insert(task_id.clone(), thread_id);
        Ok(task_id)
    }

    pub async fn attach_abort_handle(&self, task_id: &str, abort: tokio::task::AbortHandle) {
        if let Some(thread_id) = self.task_thread_map.lock().await.get(task_id).cloned() {
            if let Ok(mgr) = self.get_thread(&thread_id).await {
                mgr.attach_abort_handle(task_id, abort).await;
            }
        }
    }

    pub async fn enable_completion_notification(&self, task_id: &str) -> Option<ToolTaskRecord> {
        let thread_id = self.task_thread_map.lock().await.get(task_id)?.clone();
        let mgr = self.get_thread(&thread_id).await.ok()?;
        mgr.enable_completion_notification(task_id).await
    }

    pub async fn promote_to_background(
        &self,
        task_id: &str,
        notify: bool,
    ) -> Option<ToolTaskRecord> {
        let thread_id = self.task_thread_map.lock().await.get(task_id)?.clone();
        let mgr = self.get_thread(&thread_id).await.ok()?;
        mgr.promote_to_background(task_id, notify).await
    }

    pub async fn remove_foreground(&self, task_id: &str) {
        let thread_id = self.task_thread_map.lock().await.remove(task_id);
        if let Some(thread_id) = thread_id {
            if let Ok(mgr) = self.get_thread(&thread_id).await {
                mgr.remove_foreground(task_id).await;
            }
        }
    }

    pub async fn claim_completion_notification(&self, task_id: &str) -> bool {
        let thread_id = match self.task_thread_map.lock().await.get(task_id).cloned() {
            Some(t) => t,
            None => return false,
        };
        let mgr = match self.get_thread(&thread_id).await {
            Ok(m) => m,
            Err(_) => return false,
        };
        mgr.claim_completion_notification(task_id).await
    }

    pub async fn claim_pending_completion_notification(
        &self,
        thread_id: &str,
    ) -> Option<ToolTaskRecord> {
        let mgr = self.get_thread(thread_id).await.ok()?;
        mgr.claim_pending_completion_notification().await
    }

    pub async fn append_output(&self, task_id: &str, line: impl Into<String>) {
        if let Some(thread_id) = self.task_thread_map.lock().await.get(task_id).cloned() {
            if let Ok(mgr) = self.get_thread(&thread_id).await {
                mgr.append_output(task_id, line).await;
            }
        }
    }

    pub async fn finish(
        &self,
        task_id: &str,
        success: bool,
        elapsed_ms: u64,
        result_preview: String,
    ) -> Option<ToolTaskRecord> {
        let thread_id = self.task_thread_map.lock().await.get(task_id)?.clone();
        let mgr = self.get_thread(&thread_id).await.ok()?;
        mgr.finish(task_id, success, elapsed_ms, result_preview)
            .await
    }

    pub async fn subscribe_completed(
        &self,
        thread_id: &str,
    ) -> broadcast::Receiver<ToolTaskRecord> {
        let mgr = self.get_thread(thread_id).await.unwrap();
        mgr.completed_tx.subscribe()
    }

    pub async fn subscribe_side_events(
        &self,
        thread_id: &str,
    ) -> broadcast::Receiver<(String, CatEvent)> {
        let mgr = self.get_thread(thread_id).await.unwrap();
        mgr.side_event_tx.subscribe()
    }

    pub async fn publish_side_event(&self, thread_id: String, event: CatEvent) {
        if let Ok(mgr) = self.get_thread(&thread_id).await {
            let _ = mgr.side_event_tx.send((thread_id, event));
        }
    }

    pub async fn cancel(&self, task_id: &str) -> Option<ToolTaskRecord> {
        let thread_id = self.task_thread_map.lock().await.get(task_id)?.clone();
        let mgr = self.get_thread(&thread_id).await.ok()?;
        mgr.cancel(task_id).await
    }

    pub async fn cancel_thread(&self, thread_id: &str) -> Vec<ToolTaskRecord> {
        let mgr = match self.get_thread(thread_id).await {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };
        mgr.cancel_all_running().await
    }

    pub async fn list(&self, thread_id: Option<&str>) -> Vec<ToolTaskRecord> {
        if let Some(tid) = thread_id {
            match self.get_thread(tid).await {
                Ok(mgr) => mgr.list().await,
                Err(_) => Vec::new(),
            }
        } else {
            // List all threads
            let threads = self.threads.lock().await;
            let mut all = Vec::new();
            for mgr in threads.values() {
                all.extend(mgr.list().await);
            }
            all.sort_by(|a, b| b.started_at.cmp(&a.started_at));
            all
        }
    }

    pub async fn list_session_background(&self, thread_id: &str) -> Vec<ToolTaskRecord> {
        let records = self.list(Some(thread_id)).await;
        let mut running = records
            .iter()
            .filter(|task| task.background && task.status == TOOL_TASK_RUNNING)
            .cloned()
            .collect::<Vec<_>>();
        running.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        let mut completed = records
            .into_iter()
            .filter(|task| task.background && task.status != TOOL_TASK_RUNNING)
            .collect::<Vec<_>>();
        completed.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));
        completed.truncate(5);
        running.extend(completed);
        running
    }

    pub async fn get(&self, task_id: &str) -> Option<ToolTaskRecord> {
        let thread_id = self.task_thread_map.lock().await.get(task_id)?.clone();
        let mgr = self.get_thread(&thread_id).await.ok()?;
        mgr.get(task_id).await
    }

    pub async fn is_thread_running(&self, thread_id: &str) -> bool {
        match self.get_thread(thread_id).await {
            Ok(mgr) => mgr.is_thread_running().await,
            Err(_) => false,
        }
    }
}

fn thread_store_path(data_dir: &Path, thread_id: &str) -> PathBuf {
    let digest = Sha256::digest(thread_id.as_bytes());
    let name = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    data_dir.join(STORE_DIR).join(format!("{name}.json"))
}

fn normalize_loaded_store(store: &mut ToolTaskStore) {
    store.tasks.retain(|_, task| task.background);
    let now = Utc::now().to_rfc3339();
    for task in store.tasks.values_mut() {
        if task.status == TOOL_TASK_RUNNING {
            task.status = TOOL_TASK_CANCELLED.to_string();
            task.completed_at = Some(now.clone());
            task.success = Some(false);
            task.message = Some("cancelled because remi-cat restarted".to_string());
        }
        task.notification_delivered = true;
    }
    prune_completed_tasks(store);
}

fn prune_completed_tasks(store: &mut ToolTaskStore) {
    let mut completed = store
        .tasks
        .values()
        .filter(|task| task.background && task.status != TOOL_TASK_RUNNING)
        .map(|task| {
            (
                task.completed_at.as_deref().unwrap_or_default().to_string(),
                task.task_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    if completed.len() <= MAX_COMPLETED_TASKS {
        return;
    }
    completed.sort_unstable();
    let remove_count = completed.len() - MAX_COMPLETED_TASKS;
    for (_, task_id) in completed.into_iter().take(remove_count) {
        store.tasks.remove(&task_id);
    }
}

fn save_store(path: &Path, store: &ToolTaskStore) -> Result<()> {
    save_store_raw(path, serde_json::to_string_pretty(store)?)
}

fn save_store_raw(path: &Path, raw: String) -> Result<()> {
    let parent = path
        .parent()
        .context("tool task store path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(".tool-tasks-{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temp_path, raw)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

#[derive(Clone)]
pub struct ToolTasksTool {
    manager: Arc<ToolTaskManager>,
}

impl ToolTasksTool {
    pub fn new(manager: Arc<ToolTaskManager>) -> Self {
        Self { manager }
    }
}

impl Tool for ToolTasksTool {
    fn name(&self) -> &str {
        "tool_tasks"
    }

    fn description(&self) -> &str {
        "List, inspect, or cancel background tool tasks. Background tasks notify the agent automatically when they finish; do not poll continuously."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list", "get", "cancel"] },
                "task_id": { "type": "string", "description": "Required for get and cancel." }
            },
            "required": ["action"]
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: ToolContext,
    ) -> impl std::future::Future<
        Output = Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError>,
    > {
        let manager = Arc::clone(&self.manager);
        async move {
            let action = arguments["action"]
                .as_str()
                .unwrap_or("list")
                .trim()
                .to_ascii_lowercase();
            let thread_id = ctx.thread_id().0.clone();
            Ok(ToolResult::Output(
                stream! {
                    let value = match action.as_str() {
                        "list" => serde_json::to_value(manager.list_session_background(&thread_id).await)
                            .unwrap_or_else(|_| serde_json::json!([])),
                        "get" => {
                            let task_id = arguments["task_id"].as_str().unwrap_or_default();
                            let task = manager.get(task_id).await;
                            let task = task.filter(|task| task.thread_id == thread_id && task.background);
                            serde_json::to_value(task)
                                .unwrap_or(serde_json::Value::Null)
                        }
                        "cancel" => {
                            let task_id = arguments["task_id"].as_str().unwrap_or_default();
                            let task = manager.get(task_id).await;
                            let task = if task
                                .as_ref()
                                .is_some_and(|task| task.thread_id == thread_id && task.background)
                            {
                                manager.cancel(task_id).await
                            } else {
                                None
                            };
                            serde_json::to_value(task)
                                .unwrap_or(serde_json::Value::Null)
                        }
                        other => serde_json::json!({"error": format!("unsupported action `{other}`")}),
                    };
                    yield ToolOutput::text(serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()));
                }
                .boxed(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_data_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "remi-cat-tool-tasks-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stored_task(task_id: &str, thread_id: &str) -> ToolTaskRecord {
        ToolTaskRecord {
            task_id: task_id.to_string(),
            thread_id: thread_id.to_string(),
            run_id: "run-a".to_string(),
            app_id: None,
            tool_call_id: format!("call-{task_id}"),
            tool_name: "bash".to_string(),
            args: serde_json::json!({}),
            status: TOOL_TASK_COMPLETED.to_string(),
            background: true,
            started_at: "2026-07-09T00:00:00Z".to_string(),
            completed_at: Some("2026-07-09T00:00:01Z".to_string()),
            elapsed_ms: Some(1_000),
            success: Some(true),
            result_preview: Some("done".to_string()),
            recent_output: Vec::new(),
            message: None,
            notify_on_finish: false,
            notification_delivered: true,
        }
    }

    #[tokio::test]
    async fn manager_tracks_filters_finishes_and_cancels_tasks() {
        let dir = temp_data_dir("state");
        let manager = ToolTaskManager::load(&dir).unwrap();

        let first = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-a".to_string(),
                "bash".to_string(),
                serde_json::json!({"command": "sleep 60"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let second_cancel = CancellationToken::new();
        let second = manager
            .start(
                "thread-b".to_string(),
                "run-b".to_string(),
                "call-b".to_string(),
                "ssh".to_string(),
                serde_json::json!({"command": "sleep 60"}),
                second_cancel.clone(),
            )
            .await
            .unwrap();
        manager.promote_to_background(&first, false).await.unwrap();
        manager.promote_to_background(&second, false).await.unwrap();

        assert_eq!(manager.list(Some("thread-a")).await.len(), 1);
        assert!(manager.is_thread_running("thread-a").await);
        assert!(manager.is_thread_running("thread-b").await);

        for index in 0..25 {
            manager.append_output(&first, format!("line-{index}")).await;
        }
        let first_record = manager.get(&first).await.unwrap();
        assert_eq!(first_record.recent_output.len(), 20);
        assert_eq!(first_record.recent_output.first().unwrap(), "line-5");
        assert_eq!(first_record.recent_output.last().unwrap(), "line-24");

        let finished = manager
            .finish(&first, true, 123, "done".to_string())
            .await
            .unwrap();
        assert_eq!(finished.status, TOOL_TASK_COMPLETED);
        assert_eq!(finished.elapsed_ms, Some(123));
        assert_eq!(finished.result_preview.as_deref(), Some("done"));
        assert!(!manager.is_thread_running("thread-a").await);

        let cancelled = manager.cancel_thread("thread-b").await;
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].task_id, second);
        assert_eq!(cancelled[0].status, TOOL_TASK_CANCELLED);
        assert!(second_cancel.is_cancelled());
        assert!(!manager.is_thread_running("thread-b").await);
    }

    #[tokio::test]
    async fn output_is_persisted_only_when_task_finishes() {
        let dir = temp_data_dir("output-boundary");
        let manager = ToolTaskManager::load(&dir).unwrap();
        let task_id = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-a".to_string(),
                "bash".to_string(),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        manager
            .promote_to_background(&task_id, false)
            .await
            .unwrap();

        manager.append_output(&task_id, "cached output").await;
        let before_finish = ToolTaskManager::load(&dir).unwrap();
        assert!(before_finish
            .get(&task_id)
            .await
            .unwrap()
            .recent_output
            .is_empty());

        manager
            .finish(&task_id, true, 10, "done".to_string())
            .await
            .unwrap();
        let after_finish = ToolTaskManager::load(&dir).unwrap();
        assert_eq!(
            after_finish.get(&task_id).await.unwrap().recent_output,
            vec!["cached output"]
        );
    }

    #[tokio::test]
    async fn persistence_keeps_only_the_newest_completed_tasks() {
        let dir = temp_data_dir("retention");
        let manager = ToolTaskManager::load(&dir).unwrap();
        // Create and finish MAX_COMPLETED_TASKS + 1 tasks
        for index in 0..=MAX_COMPLETED_TASKS {
            let task_id = manager
                .start(
                    "thread-a".to_string(),
                    "run-a".to_string(),
                    format!("call-{index}"),
                    "bash".to_string(),
                    serde_json::json!({}),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            manager
                .promote_to_background(&task_id, false)
                .await
                .unwrap();
            manager
                .finish(&task_id, true, 1, format!("result-{index}"))
                .await;
        }
        // Reload to trigger pruning
        let manager = ToolTaskManager::load(&dir).unwrap();
        let tasks = manager.list(Some("thread-a")).await;
        assert_eq!(tasks.len(), MAX_COMPLETED_TASKS);
        // The oldest task should have been pruned
        assert!(manager.get(&tasks.last().unwrap().task_id).await.is_some());
    }

    #[tokio::test]
    async fn concurrent_finishes_persist_the_latest_combined_state() {
        let dir = temp_data_dir("concurrent-finish");
        let manager = ToolTaskManager::load(&dir).unwrap();
        let first = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-a".to_string(),
                "bash".to_string(),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let second = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-b".to_string(),
                "bash".to_string(),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        manager.promote_to_background(&first, false).await.unwrap();
        manager.promote_to_background(&second, false).await.unwrap();

        tokio::join!(
            manager.finish(&first, true, 1, "first".to_string()),
            manager.finish(&second, true, 1, "second".to_string()),
        );

        let reloaded = ToolTaskManager::load(&dir).unwrap();
        assert_eq!(
            reloaded.get(&first).await.unwrap().status,
            TOOL_TASK_COMPLETED
        );
        assert_eq!(
            reloaded.get(&second).await.unwrap().status,
            TOOL_TASK_COMPLETED
        );
    }

    #[tokio::test]
    async fn load_marks_running_tasks_cancelled_after_restart() {
        let dir = temp_data_dir("restart");
        let manager = ToolTaskManager::load(&dir).unwrap();
        let task_id = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-a".to_string(),
                "bash".to_string(),
                serde_json::json!({"command": "sleep 60"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        manager
            .promote_to_background(&task_id, false)
            .await
            .unwrap();
        drop(manager);

        let reloaded = ToolTaskManager::load(&dir).unwrap();
        let task = reloaded.get(&task_id).await.unwrap();
        assert_eq!(task.status, TOOL_TASK_CANCELLED);
        assert_eq!(task.success, Some(false));
        assert_eq!(
            task.message.as_deref(),
            Some("cancelled because remi-cat restarted")
        );
        assert!(!reloaded.is_thread_running("thread-a").await);
    }

    #[tokio::test]
    async fn finish_does_not_override_cancelled_task() {
        let manager = ToolTaskManager::load(temp_data_dir("cancel-finish")).unwrap();
        let task_id = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-a".to_string(),
                "bash".to_string(),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        manager.promote_to_background(&task_id, true).await.unwrap();

        manager.cancel(&task_id).await.unwrap();
        let finished = manager
            .finish(&task_id, true, 10, "late result".to_string())
            .await
            .unwrap();

        assert_eq!(finished.status, TOOL_TASK_CANCELLED);
        assert_eq!(finished.result_preview, None);
        assert_eq!(
            manager.get(&task_id).await.unwrap().status,
            TOOL_TASK_CANCELLED
        );
    }

    #[tokio::test]
    async fn enable_completion_notification_replays_already_completed_task() {
        let manager = ToolTaskManager::load(temp_data_dir("late-enable")).unwrap();
        let mut completed_rx = manager.subscribe_completed("thread-a").await;
        let task_id = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-a".to_string(),
                "bash".to_string(),
                serde_json::json!({"command": "sleep 10"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        manager
            .finish(&task_id, true, 10_000, "done".to_string())
            .await
            .unwrap();
        let replayed = manager
            .enable_completion_notification(&task_id)
            .await
            .expect("completed task should be returned");

        assert_eq!(replayed.task_id, task_id);
        let completed = completed_rx.recv().await.unwrap();
        assert_eq!(completed.task_id, task_id);
        assert_eq!(completed.status, TOOL_TASK_COMPLETED);
    }

    #[tokio::test]
    async fn completion_notification_claim_is_idempotent() {
        let manager = ToolTaskManager::load(temp_data_dir("claim")).unwrap();
        let mut completed_rx = manager.subscribe_completed("thread-a").await;
        let task_id = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-a".to_string(),
                "bash".to_string(),
                serde_json::json!({"command": "sleep 10"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        manager.promote_to_background(&task_id, true).await.unwrap();
        manager.enable_completion_notification(&task_id).await;
        manager
            .finish(&task_id, true, 10_000, "done".to_string())
            .await
            .unwrap();

        assert!(manager.claim_completion_notification(&task_id).await);
        assert!(!manager.claim_completion_notification(&task_id).await);
        assert!(manager
            .claim_pending_completion_notification("thread-a")
            .await
            .is_none());
        assert_eq!(completed_rx.recv().await.unwrap().task_id, task_id);
    }

    #[tokio::test]
    async fn restart_does_not_replay_historical_completion_notification() {
        let dir = temp_data_dir("restart-notification");
        let manager = ToolTaskManager::load(&dir).unwrap();
        let task_id = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-a".to_string(),
                "bash".to_string(),
                serde_json::json!({}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        manager.promote_to_background(&task_id, true).await.unwrap();
        manager.enable_completion_notification(&task_id).await;
        manager
            .finish(&task_id, true, 10, "done".to_string())
            .await
            .unwrap();
        drop(manager);

        let reloaded = ToolTaskManager::load(&dir).unwrap();
        assert!(reloaded
            .claim_pending_completion_notification("thread-a")
            .await
            .is_none());
        assert!(!reloaded.claim_completion_notification(&task_id).await);
    }

    #[test]
    fn legacy_record_defaults_to_background() {
        let record: ToolTaskRecord = serde_json::from_value(serde_json::json!({
            "task_id": "legacy",
            "thread_id": "thread-a",
            "run_id": "run-a",
            "tool_call_id": "call-a",
            "tool_name": "bash",
            "args": {},
            "status": "completed",
            "started_at": "2026-07-09T00:00:00Z"
        }))
        .unwrap();

        assert!(record.background);
    }

    #[tokio::test]
    async fn foreground_records_are_hidden_and_removed() {
        let manager = ToolTaskManager::load(temp_data_dir("foreground-cleanup")).unwrap();
        let task_id = manager
            .start(
                "thread-a".to_string(),
                "run-a".to_string(),
                "call-a".to_string(),
                "bash".to_string(),
                serde_json::json!({"command": "true"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(manager.list(Some("thread-a")).await.is_empty());
        manager.remove_foreground(&task_id).await;
        assert!(manager.get(&task_id).await.is_none());
    }

    #[tokio::test]
    async fn session_list_keeps_all_running_and_latest_five_finished() {
        let manager = ToolTaskManager::load(temp_data_dir("session-summary")).unwrap();
        for index in 0..2 {
            let task_id = manager
                .start(
                    "thread-a".to_string(),
                    "run-a".to_string(),
                    format!("running-{index}"),
                    "bash".to_string(),
                    serde_json::json!({"index": index}),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            manager
                .promote_to_background(&task_id, false)
                .await
                .unwrap();
        }
        for index in 0..7 {
            let task_id = manager
                .start(
                    "thread-a".to_string(),
                    "run-a".to_string(),
                    format!("finished-{index}"),
                    "bash".to_string(),
                    serde_json::json!({"index": index}),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            manager
                .promote_to_background(&task_id, false)
                .await
                .unwrap();
            manager
                .finish(&task_id, true, index, format!("result-{index}"))
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        let tasks = manager.list_session_background("thread-a").await;
        assert_eq!(tasks.len(), 7);
        assert_eq!(
            tasks
                .iter()
                .filter(|task| task.status == TOOL_TASK_RUNNING)
                .count(),
            2
        );
        assert_eq!(
            tasks
                .iter()
                .filter(|task| task.status != TOOL_TASK_RUNNING)
                .count(),
            5
        );
        assert!(tasks.iter().all(|task| task.thread_id == "thread-a"));
    }

    #[tokio::test]
    async fn colliding_legacy_thread_names_migrate_to_distinct_files() {
        let dir = temp_data_dir("thread-hash");
        let legacy_path = dir.join(STORE_DIR).join("team_a.json");
        let legacy = ToolTaskStore {
            tasks: HashMap::from([
                (
                    "task-slash".to_string(),
                    stored_task("task-slash", "team/a"),
                ),
                (
                    "task-colon".to_string(),
                    stored_task("task-colon", "team:a"),
                ),
            ]),
        };
        save_store(&legacy_path, &legacy).unwrap();

        let reloaded = ToolTaskManager::load(&dir).unwrap();
        assert_eq!(reloaded.list(Some("team/a")).await.len(), 1);
        assert_eq!(reloaded.list(Some("team:a")).await.len(), 1);
        assert_ne!(
            thread_store_path(&dir, "team/a"),
            thread_store_path(&dir, "team:a")
        );
        assert!(!legacy_path.exists());
    }
}
