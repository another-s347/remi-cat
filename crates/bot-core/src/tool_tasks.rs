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

const STORE_FILE: &str = "tool_tasks.json";
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
pub struct ToolTaskManager {
    path: PathBuf,
    store: Mutex<ToolTaskStore>,
    running: Mutex<HashMap<String, RunningTask>>,
    completed_tx: broadcast::Sender<ToolTaskRecord>,
    side_event_tx: broadcast::Sender<(String, CatEvent)>,
}

impl ToolTaskManager {
    pub fn load(data_dir: impl AsRef<Path>) -> Result<Arc<Self>> {
        let path = data_dir.as_ref().join(STORE_FILE);
        let mut store = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<ToolTaskStore>(&raw)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => ToolTaskStore::default(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let now = Utc::now().to_rfc3339();
        for task in store.tasks.values_mut() {
            if task.status == TOOL_TASK_RUNNING {
                task.status = TOOL_TASK_CANCELLED.to_string();
                task.completed_at = Some(now.clone());
                task.success = Some(false);
                task.message = Some("cancelled because remi-cat restarted".to_string());
            }
        }
        save_store(&path, &store)?;
        let manager = Arc::new(Self {
            path,
            store: Mutex::new(store),
            running: Mutex::new(HashMap::new()),
            completed_tx: broadcast::channel(64).0,
            side_event_tx: broadcast::channel(256).0,
        });
        Ok(manager)
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
        let task_id = uuid::Uuid::new_v4().to_string();
        let record = ToolTaskRecord {
            task_id: task_id.clone(),
            thread_id,
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
        self.save().await?;
        Ok(task_id)
    }

    pub async fn attach_abort_handle(&self, task_id: &str, abort: tokio::task::AbortHandle) {
        if let Some(running) = self.running.lock().await.get_mut(task_id) {
            running.abort = Some(abort);
        } else {
            abort.abort();
        }
    }

    pub async fn enable_completion_notification(&self, task_id: &str) -> Option<ToolTaskRecord> {
        let mut store = self.store.lock().await;
        let record = store.tasks.get_mut(task_id)?;
        record.notify_on_finish = true;
        let snapshot = record.clone();
        drop(store);
        let _ = self.save().await;
        if snapshot.status == TOOL_TASK_RUNNING {
            return None;
        }
        if !snapshot.notification_delivered {
            let _ = self.completed_tx.send(snapshot.clone());
        }
        Some(snapshot)
    }

    pub async fn claim_completion_notification(&self, task_id: &str) -> bool {
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
        drop(store);
        let _ = self.save().await;
        true
    }

    pub async fn claim_pending_completion_notification(
        &self,
        thread_id: &str,
    ) -> Option<ToolTaskRecord> {
        let mut store = self.store.lock().await;
        for record in store.tasks.values_mut() {
            if record.thread_id == thread_id
                && record.status != TOOL_TASK_RUNNING
                && record.notify_on_finish
                && !record.notification_delivered
            {
                record.notification_delivered = true;
                let pending = record.clone();
                drop(store);
                let _ = self.save().await;
                return Some(pending);
            }
        }
        drop(store);
        None
    }

    pub async fn append_output(&self, task_id: &str, line: impl Into<String>) {
        let mut store = self.store.lock().await;
        if let Some(record) = store.tasks.get_mut(task_id) {
            record.recent_output.push(line.into());
            if record.recent_output.len() > 20 {
                let drop_count = record.recent_output.len() - 20;
                record.recent_output.drain(0..drop_count);
            }
        }
        drop(store);
        let _ = self.save().await;
    }

    pub async fn finish(
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
        let notify_on_finish = record.notify_on_finish && !record.notification_delivered;
        let cloned = record.clone();
        drop(store);
        let _ = self.save().await;
        if notify_on_finish {
            let _ = self.completed_tx.send(cloned.clone());
        }
        Some(cloned)
    }

    pub fn subscribe_completed(&self) -> broadcast::Receiver<ToolTaskRecord> {
        self.completed_tx.subscribe()
    }

    pub fn subscribe_side_events(&self) -> broadcast::Receiver<(String, CatEvent)> {
        self.side_event_tx.subscribe()
    }

    pub fn publish_side_event(&self, thread_id: String, event: CatEvent) {
        let _ = self.side_event_tx.send((thread_id, event));
    }

    pub async fn cancel(&self, task_id: &str) -> Option<ToolTaskRecord> {
        if let Some(running) = self.running.lock().await.remove(task_id) {
            running.cancel.cancel();
            if let Some(abort) = running.abort {
                abort.abort();
            }
        }
        let mut store = self.store.lock().await;
        let record = store.tasks.get_mut(task_id)?;
        if record.status == TOOL_TASK_RUNNING {
            record.status = TOOL_TASK_CANCELLED.to_string();
            record.completed_at = Some(Utc::now().to_rfc3339());
            record.success = Some(false);
            record.message = Some("cancelled".to_string());
        }
        let cloned = record.clone();
        drop(store);
        let _ = self.save().await;
        Some(cloned)
    }

    pub async fn cancel_thread(&self, thread_id: &str) -> Vec<ToolTaskRecord> {
        let ids = self
            .list(Some(thread_id))
            .await
            .into_iter()
            .filter(|task| task.status == TOOL_TASK_RUNNING)
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        let mut cancelled = Vec::new();
        for id in ids {
            if let Some(task) = self.cancel(&id).await {
                cancelled.push(task);
            }
        }
        cancelled
    }

    pub async fn list(&self, thread_id: Option<&str>) -> Vec<ToolTaskRecord> {
        let mut records = self
            .store
            .lock()
            .await
            .tasks
            .values()
            .filter(|task| thread_id.map(|id| id == task.thread_id).unwrap_or(true))
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        records
    }

    pub async fn get(&self, task_id: &str) -> Option<ToolTaskRecord> {
        self.store.lock().await.tasks.get(task_id).cloned()
    }

    pub async fn is_thread_running(&self, thread_id: &str) -> bool {
        self.store
            .lock()
            .await
            .tasks
            .values()
            .any(|task| task.thread_id == thread_id && task.status == TOOL_TASK_RUNNING)
    }

    async fn save(&self) -> Result<()> {
        let store = self.store.lock().await;
        save_store(&self.path, &store)
    }
}

fn save_store(path: &Path, store: &ToolTaskStore) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(store)?;
    std::fs::write(path, raw)?;
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
        let mut completed_rx = manager.subscribe_completed();
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
        let mut completed_rx = manager.subscribe_completed();
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
}
