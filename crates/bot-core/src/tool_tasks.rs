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
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub status: String,
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
    ) -> String {
        let task_id = uuid::Uuid::new_v4().to_string();
        let record = ToolTaskRecord {
            task_id: task_id.clone(),
            thread_id: self.thread_id.clone(),
            run_id,
            tool_call_id,
            tool_name,
            args,
            status: TOOL_TASK_RUNNING.to_string(),
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
            record.status = TOOL_TASK_CANCELLED.to_string();
            record.completed_at = Some(Utc::now().to_rfc3339());
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
        let mut records: Vec<ToolTaskRecord> =
            self.store.lock().await.tasks.values().cloned().collect();
        records.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        records
    }

    async fn get(&self, task_id: &str) -> Option<ToolTaskRecord> {
        self.store.lock().await.tasks.get(task_id).cloned()
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
        std::fs::create_dir_all(data_dir.join(STORE_DIR))
            .with_context(|| format!("creating tool task dir"))?;
        // Migrate legacy shared file into per-thread directory
        let legacy = data_dir.join(LEGACY_STORE_FILE);
        if legacy.exists() {
            let dest = data_dir.join(STORE_DIR).join("_global.json");
            if !dest.exists() {
                let _ = std::fs::copy(&legacy, &dest);
            }
            let _ = std::fs::remove_file(&legacy);
        }
        let mut threads = HashMap::new();
        let mut task_thread_map = HashMap::new();
        for entry in std::fs::read_dir(data_dir.join(STORE_DIR))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json")
                || path.file_name().and_then(|value| value.to_str()) == Some("_global.json")
            {
                continue;
            }
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let store = serde_json::from_str::<ToolTaskStore>(&raw)
                .with_context(|| format!("parsing {}", path.display()))?;
            let thread_id = store
                .tasks
                .values()
                .find_map(|task| (!task.thread_id.is_empty()).then(|| task.thread_id.clone()))
                .or_else(|| {
                    path.file_stem()
                        .and_then(|value| value.to_str())
                        .map(ToOwned::to_owned)
                });
            let Some(thread_id) = thread_id else {
                continue;
            };
            let task_ids = store.tasks.keys().cloned().collect::<Vec<_>>();
            let manager = Self::load_per_thread(&data_dir, &thread_id)?;
            for task_id in task_ids {
                task_thread_map.insert(task_id, thread_id.clone());
            }
            threads.insert(thread_id, manager);
        }
        Ok(Arc::new(Self {
            data_dir,
            threads: Mutex::new(threads),
            task_thread_map: Mutex::new(task_thread_map),
        }))
    }

    fn safe_thread_name(thread_id: &str) -> String {
        thread_id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn load_per_thread(data_dir: &Path, thread_id: &str) -> Result<Arc<PerThreadManager>> {
        let safe_name = Self::safe_thread_name(thread_id);
        let path = data_dir.join(STORE_DIR).join(format!("{safe_name}.json"));
        let mut store = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<ToolTaskStore>(&raw)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ToolTaskStore::default(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let now = Utc::now().to_rfc3339();
        for task in store.tasks.values_mut() {
            if task.thread_id.is_empty() {
                task.thread_id = thread_id.to_string();
            }
            if task.status == TOOL_TASK_RUNNING {
                task.status = TOOL_TASK_CANCELLED.to_string();
                task.completed_at = Some(now.clone());
                task.success = Some(false);
                task.message = Some("cancelled because remi-cat restarted".to_string());
            }
            task.notification_delivered = true;
        }
        prune_completed_tasks(&mut store);
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
            .start_inner(run_id, tool_call_id, tool_name, args, cancel)
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

fn prune_completed_tasks(store: &mut ToolTaskStore) {
    let mut completed = store
        .tasks
        .values()
        .filter(|task| task.status != TOOL_TASK_RUNNING)
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
                        "list" => serde_json::to_value(manager.list(Some(&thread_id)).await)
                            .unwrap_or_else(|_| serde_json::json!([])),
                        "get" => {
                            let task_id = arguments["task_id"].as_str().unwrap_or_default();
                            let task = manager.get(task_id).await;
                            let task = task.filter(|task| task.thread_id == thread_id);
                            serde_json::to_value(task)
                                .unwrap_or(serde_json::Value::Null)
                        }
                        "cancel" => {
                            let task_id = arguments["task_id"].as_str().unwrap_or_default();
                            let task = manager.get(task_id).await;
                            let task = if task
                                .as_ref()
                                .is_some_and(|task| task.thread_id == thread_id)
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
}
