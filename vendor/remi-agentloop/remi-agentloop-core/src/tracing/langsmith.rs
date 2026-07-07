//! LangSmith tracing backend.
//!
//! Posts a run tree to the [LangSmith](https://docs.smith.langchain.com/) REST
//! API.  All HTTP calls are dispatched through an internal mpsc channel to a
//! background Tokio task so tracing **never blocks** the agent loop.
//!
//! Requires the `tracing-langsmith` feature flag.
//!
//! # Example
//! ```no_run
//! use remi_agentloop::tracing::LangSmithTracer;
//!
//! let tracer = LangSmithTracer::new(std::env::var("LANGSMITH_API_KEY").unwrap())
//!     .with_project("my-agent");
//! ```

use std::collections::HashMap;
use std::future::Future;

use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::config::AgentConfig;
use crate::tracing::{
    ExternalToolResultTrace, InterruptTrace, ModelEndTrace, ModelStartTrace, ResumeTrace,
    RunEndTrace, RunStartTrace, RunStatus, ToolEndTrace, ToolExecutionHandoffTrace, ToolStartTrace,
    Tracer, TurnStartTrace,
};

const DEFAULT_API_URL: &str = "https://api.smith.langchain.com";

// ── Internal message passed to the background sender ─────────────────────────

enum LangSmithMessage {
    /// Create a new run (HTTP POST /runs).
    Post {
        run: LangSmithRun,
        api_url: String,
        api_key: String,
    },
    /// Update an existing run (HTTP PATCH /runs/{id}).
    Patch {
        id: String,
        patch: serde_json::Value,
        api_url: String,
        api_key: String,
    },
    Flush {
        close: bool,
        ack: oneshot::Sender<()>,
    },
}

// ── LangSmith run payload ─────────────────────────────────────────────────────

/// Wire format for POST /runs.
#[derive(Debug, Serialize)]
struct LangSmithRun {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_run_id: Option<String>,
    run_type: String,
    name: String,
    inputs: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    outputs: Option<serde_json::Value>,
    start_time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    extra: Option<serde_json::Value>,
    session_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage_metadata: Option<serde_json::Value>,
}

// ── LangSmithTracer ───────────────────────────────────────────────────────────

/// LangSmith tracing backend.
///
/// Maps the remi-agentloop trace events onto LangSmith's run-tree model:
///
/// ```text
/// Chain Run  (AgentLoop run, run_id)
///   ├── LLM Run   (model call, turn 0)
///   ├── Tool Run  (tool execution)
///   ├── LLM Run   (model call, turn 1)
///   └── ...
/// ```
///
/// Interrupt/resume is tracked as a **single** Chain Run — `on_interrupt`
/// patches the run status, `on_resume` clears `end_time` so the run shows
/// as still in-progress, and subsequent events keep appending to the same
/// chain.
pub struct LangSmithTracer {
    api_key: String,
    api_url: String,
    project_name: String,
    manage_root_run: bool,
    /// Channel sender — wrapped in Option so terminal close can detach it.
    tx: std::sync::Mutex<Option<mpsc::UnboundedSender<LangSmithMessage>>>,
    /// Handle to the background sender task.
    handle: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    custom_event_counters: std::sync::Mutex<HashMap<String, usize>>,
}

impl LangSmithTracer {
    pub fn is_closed(&self) -> bool {
        self.tx
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(mpsc::UnboundedSender::is_closed))
            .unwrap_or(true)
    }

    fn log_enqueue_error(operation: &str, target: &str, error: &str) {
        eprintln!("[langsmith] failed to enqueue {operation} for {target}: {error}");
    }

    async fn log_http_failure(operation: &str, target: &str, response: reqwest::Response) {
        let status = response.status();
        let body = match response.text().await {
            Ok(text) if !text.trim().is_empty() => text,
            Ok(_) => "<empty body>".to_string(),
            Err(error) => format!("<failed to read body: {error}>"),
        };

        eprintln!("[langsmith] {operation} failed for {target}: status={status}, body={body}");
    }

    /// Create a new tracer.  Spawns a background Tokio task immediately.
    pub fn new(api_key: impl Into<String>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(Self::background_sender(rx));
        Self {
            api_key: api_key.into(),
            api_url: DEFAULT_API_URL.to_string(),
            project_name: "default".to_string(),
            manage_root_run: true,
            tx: std::sync::Mutex::new(Some(tx)),
            handle: std::sync::Mutex::new(Some(handle)),
            custom_event_counters: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Reattach beneath an already-existing root run without creating or patching it.
    pub fn attach_to_existing_run(mut self) -> Self {
        self.manage_root_run = false;
        self
    }

    async fn send_flush_signal(&self, close: bool) {
        let (ack_tx, ack_rx) = oneshot::channel();
        let send_result = if let Ok(guard) = self.tx.lock() {
            if let Some(tx) = guard.as_ref() {
                tx.send(LangSmithMessage::Flush { close, ack: ack_tx })
                    .map_err(|error| error.to_string())
            } else {
                Err("sender channel already closed".to_string())
            }
        } else {
            Err("sender mutex poisoned".to_string())
        };

        if let Err(error) = send_result {
            Self::log_enqueue_error(
                if close { "flush(close)" } else { "flush" },
                "tracer queue",
                &error,
            );
            return;
        }

        if ack_rx.await.is_err() {
            eprintln!(
                "[langsmith] background sender dropped before acknowledging {}",
                if close { "close" } else { "flush" }
            );
        }
    }

    /// Wait for all currently queued HTTP calls to finish, without closing the tracer.
    ///
    /// This is safe to call between interrupted/resumed turns.
    pub async fn flush(&self) {
        self.send_flush_signal(false).await;
    }

    /// Flush pending work and permanently close the background sender.
    ///
    /// Call this only when the run is terminal and no further trace events will be emitted.
    pub async fn close(&self) {
        self.send_flush_signal(true).await;

        if let Ok(mut guard) = self.tx.lock() {
            let _ = guard.take();
        }

        let handle = self.handle.lock().ok().and_then(|mut g| g.take());
        if let Some(h) = handle {
            let _ = h.await;
        }
    }

    /// Set the LangSmith project (session) name.  Default: `"default"`.
    pub fn with_project(mut self, name: impl Into<String>) -> Self {
        self.project_name = name.into();
        self
    }

    /// Override the LangSmith API base URL.  Useful for self-hosted
    /// deployments.
    pub fn with_api_url(mut self, url: impl Into<String>) -> Self {
        self.api_url = url.into();
        self
    }

    /// Construct from an [`AgentConfig`], reading credentials from
    /// `config.extra`.
    ///
    /// Recognised keys:
    /// - `langsmith_api_key`  (required)
    /// - `langsmith_project`  (optional, default `"default"`)
    /// - `langsmith_api_url`  (optional)
    ///
    /// Returns `None` when `langsmith_api_key` is absent.
    pub fn from_config(config: &AgentConfig) -> Option<Self> {
        let api_key = config.extra.get("langsmith_api_key")?.as_str()?;
        let mut tracer = Self::new(api_key);
        if let Some(p) = config
            .extra
            .get("langsmith_project")
            .and_then(|v| v.as_str())
        {
            tracer = tracer.with_project(p);
        }
        if let Some(u) = config
            .extra
            .get("langsmith_api_url")
            .and_then(|v| v.as_str())
        {
            tracer = tracer.with_api_url(u);
        }
        Some(tracer)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn post(&self, run: LangSmithRun) {
        let target = format!("run {} ({})", run.id, run.name);
        if let Ok(guard) = self.tx.lock() {
            if let Some(tx) = guard.as_ref() {
                if let Err(error) = tx.send(LangSmithMessage::Post {
                    run,
                    api_url: self.api_url.clone(),
                    api_key: self.api_key.clone(),
                }) {
                    Self::log_enqueue_error("POST /runs", &target, &error.to_string());
                }
            } else {
                Self::log_enqueue_error("POST /runs", &target, "sender channel already closed");
            }
        } else {
            Self::log_enqueue_error("POST /runs", &target, "sender mutex poisoned");
        }
    }

    fn patch(&self, id: impl Into<String>, patch: serde_json::Value) {
        let id = id.into();
        let target = format!("run {id}");
        if let Ok(guard) = self.tx.lock() {
            if let Some(tx) = guard.as_ref() {
                if let Err(error) = tx.send(LangSmithMessage::Patch {
                    id,
                    patch,
                    api_url: self.api_url.clone(),
                    api_key: self.api_key.clone(),
                }) {
                    Self::log_enqueue_error("PATCH /runs/{id}", &target, &error.to_string());
                }
            } else {
                Self::log_enqueue_error(
                    "PATCH /runs/{id}",
                    &target,
                    "sender channel already closed",
                );
            }
        } else {
            Self::log_enqueue_error("PATCH /runs/{id}", &target, "sender mutex poisoned");
        }
    }

    fn llm_run_id(run_id: &crate::types::RunId, index: usize) -> String {
        Self::namespaced_uuid(&run_id.0, &format!("llm:{index}"))
    }

    fn namespaced_uuid(namespace: &str, name: &str) -> String {
        let ns = Uuid::parse_str(namespace).unwrap_or_else(|_| Uuid::nil());
        Uuid::new_v5(&ns, name.as_bytes()).to_string()
    }

    fn clear_run_state(&self, run_id: &crate::types::RunId) {
        if let Ok(mut guard) = self.custom_event_counters.lock() {
            guard.remove(&run_id.0);
        }
    }

    fn llm_output_tool_calls(
        tool_calls: &[crate::tracing::ToolCallTrace],
    ) -> Vec<serde_json::Value> {
        tool_calls
            .iter()
            .map(|tool_call| {
                serde_json::json!({
                    "id": tool_call.id,
                    "type": "function",
                    "function": {
                        "name": tool_call.name,
                        "arguments": serde_json::to_string(&tool_call.arguments)
                            .unwrap_or_else(|_| "{}".to_string()),
                    },
                })
            })
            .collect()
    }

    fn post_tool_run(
        &self,
        run_id: &crate::types::RunId,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &serde_json::Value,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) {
        let tool_id = Self::tool_run_id(run_id, tool_call_id);
        self.post(LangSmithRun {
            id: tool_id,
            parent_run_id: Some(run_id.0.clone()),
            run_type: "tool".to_string(),
            name: tool_name.to_string(),
            inputs: serde_json::json!({
                "arguments": arguments,
            }),
            outputs: None,
            start_time: timestamp.to_rfc3339(),
            end_time: None,
            extra: None,
            session_name: self.project_name.clone(),
            status: None,
            error: None,
            metadata: None,
            usage_metadata: None,
        });
    }

    fn patch_tool_run(
        &self,
        run_id: &crate::types::RunId,
        tool_call_id: &str,
        result: Option<&str>,
        interrupted: bool,
        error: Option<&str>,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) {
        let tool_id = Self::tool_run_id(run_id, tool_call_id);
        let status = if error.is_some() { "error" } else { "success" };
        self.patch(
            tool_id,
            serde_json::json!({
                "outputs": {
                    "result": result,
                    "interrupted": interrupted,
                },
                "end_time": timestamp.to_rfc3339(),
                "status": status,
                "error": error,
            }),
        );
    }

    /// Uses UUID v5 (namespace = parent run UUID, name = `"tool:<tool_call_id>"`) to
    /// convert the OpenAI `call_abc123` format into a valid UUID that LangSmith accepts.
    fn tool_run_id(run_id: &crate::types::RunId, tool_call_id: &str) -> String {
        Self::namespaced_uuid(&run_id.0, &format!("tool:{tool_call_id}"))
    }

    fn next_custom_event_index(&self, run_id: &str) -> usize {
        self.custom_event_counters
            .lock()
            .ok()
            .map(|mut guard| {
                let counter = guard.entry(run_id.to_string()).or_insert(0);
                let index = *counter;
                *counter += 1;
                index
            })
            .unwrap_or(0)
    }

    pub fn clear_custom_event_state(&self, run_id: &str) {
        if let Ok(mut guard) = self.custom_event_counters.lock() {
            guard.remove(run_id);
        }
    }

    pub fn post_child_run(
        &self,
        parent_run_id: &str,
        run_id: &str,
        name: &str,
        inputs: serde_json::Value,
        extra: Option<serde_json::Value>,
        metadata: Option<serde_json::Value>,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) {
        self.post(LangSmithRun {
            id: run_id.to_string(),
            parent_run_id: Some(parent_run_id.to_string()),
            run_type: "chain".to_string(),
            name: name.to_string(),
            inputs,
            outputs: None,
            start_time: timestamp.to_rfc3339(),
            end_time: None,
            extra,
            session_name: self.project_name.clone(),
            status: None,
            error: None,
            metadata,
            usage_metadata: None,
        });
    }

    pub fn patch_run_outputs(
        &self,
        run_id: &str,
        outputs: serde_json::Value,
        status: &str,
        error: Option<&str>,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) {
        self.patch(
            run_id.to_string(),
            serde_json::json!({
                "outputs": outputs,
                "end_time": timestamp.to_rfc3339(),
                "status": status,
                "error": error,
            }),
        );
    }

    pub fn post_tool_run_under_parent(
        &self,
        parent_run_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &serde_json::Value,
        timestamp: chrono::DateTime<chrono::Utc>,
    ) {
        let parent = crate::types::RunId(parent_run_id.to_string());
        self.post_tool_run(&parent, tool_call_id, tool_name, arguments, timestamp);
    }

    pub fn patch_tool_run_under_parent(
        &self,
        parent_run_id: &str,
        tool_call_id: &str,
        result: Option<&str>,
        interrupted: bool,
        error: Option<&str>,
        timestamp: chrono::DateTime<chrono::Utc>,
        arguments: Option<&serde_json::Value>,
    ) {
        let parent = crate::types::RunId(parent_run_id.to_string());
        let tool_id = Self::tool_run_id(&parent, tool_call_id);
        let status = if error.is_some() { "error" } else { "success" };
        self.patch(
            tool_id,
            serde_json::json!({
                "inputs": arguments.map(|value| serde_json::json!({ "arguments": value })),
                "outputs": {
                    "result": result,
                    "interrupted": interrupted,
                },
                "end_time": timestamp.to_rfc3339(),
                "status": status,
                "error": error,
            }),
        );
    }

    pub fn emit_custom_under(&self, parent_run_id: &str, name: &str, data: &serde_json::Value) {
        let index = self.next_custom_event_index(parent_run_id);
        let custom_id = Self::namespaced_uuid(parent_run_id, &format!("custom:{name}:{index}"));
        let timestamp = chrono::Utc::now().to_rfc3339();
        self.post(LangSmithRun {
            id: custom_id,
            parent_run_id: Some(parent_run_id.to_string()),
            run_type: "chain".to_string(),
            name: format!("custom:{name}"),
            inputs: data.clone(),
            outputs: Some(serde_json::json!({
                "custom_event": name,
            })),
            start_time: timestamp.clone(),
            end_time: Some(timestamp),
            extra: Some(serde_json::json!({
                "custom_event_name": name,
            })),
            session_name: self.project_name.clone(),
            status: Some("success".to_string()),
            error: None,
            metadata: None,
            usage_metadata: None,
        });
    }

    /// Background task: drains the mpsc channel and performs HTTP calls.
    async fn background_sender(mut rx: mpsc::UnboundedReceiver<LangSmithMessage>) {
        let client = reqwest::Client::new();
        while let Some(msg) = rx.recv().await {
            match msg {
                LangSmithMessage::Post {
                    run,
                    api_url,
                    api_key,
                } => {
                    let url = format!("{api_url}/runs");
                    let target = format!("run {} ({})", run.id, run.name);
                    match client
                        .post(&url)
                        .header("x-api-key", &api_key)
                        .json(&run)
                        .send()
                        .await
                    {
                        Ok(response) => {
                            if !response.status().is_success() {
                                Self::log_http_failure("POST /runs", &target, response).await;
                            }
                        }
                        Err(error) => {
                            eprintln!(
                                "[langsmith] POST /runs transport error for {target}: {error}"
                            );
                        }
                    }
                }
                LangSmithMessage::Patch {
                    id,
                    patch,
                    api_url,
                    api_key,
                } => {
                    let url = format!("{api_url}/runs/{id}");
                    let target = format!("run {id}");
                    match client
                        .patch(&url)
                        .header("x-api-key", &api_key)
                        .json(&patch)
                        .send()
                        .await
                    {
                        Ok(response) => {
                            if !response.status().is_success() {
                                Self::log_http_failure("PATCH /runs/{id}", &target, response).await;
                            }
                        }
                        Err(error) => {
                            eprintln!("[langsmith] PATCH /runs/{id} transport error for {target}: {error}");
                        }
                    }
                }
                LangSmithMessage::Flush { close, ack } => {
                    let _ = ack.send(());
                    if close {
                        break;
                    }
                }
            }
        }
    }
}

// ── Tracer impl ────────────────────────────────────────────────────────────────

impl Tracer for LangSmithTracer {
    /// POST a new Chain Run representing the full AgentLoop execution.
    fn on_run_start(&self, event: &RunStartTrace) -> impl Future<Output = ()> {
        async move {
            if !self.manage_root_run {
                return;
            }
            self.post(LangSmithRun {
                id: event.run_id.0.clone(),
                parent_run_id: None,
                run_type: "chain".to_string(),
                name: "AgentLoop".to_string(),
                inputs: serde_json::json!({
                    "messages": event.input_messages,
                    "model": event.model,
                }),
                outputs: None,
                start_time: event.timestamp.to_rfc3339(),
                end_time: None,
                extra: Some(serde_json::json!({
                    "system_prompt": event.system_prompt,
                    "thread_id": event.thread_id,
                })),
                session_name: self.project_name.clone(),
                status: None,
                error: None,
                metadata: event.metadata.clone(),
                usage_metadata: None,
            });
        }
    }

    /// PATCH the Chain Run with final outputs, status, and token usage.
    fn on_run_end(&self, event: &RunEndTrace) -> impl Future<Output = ()> {
        async move {
            if !self.manage_root_run {
                self.clear_run_state(&event.run_id);
                return;
            }
            self.clear_run_state(&event.run_id);
            let status = match &event.status {
                RunStatus::Completed => "success",
                RunStatus::Error | RunStatus::MaxTurnsExceeded => "error",
                RunStatus::Interrupted => "interrupted",
            };
            self.patch(
                event.run_id.0.clone(),
                serde_json::json!({
                    "outputs": {
                        "messages": event.output_messages,
                    },
                    "end_time": event.timestamp.to_rfc3339(),
                    "status": status,
                    "error": event.error,
                    "usage_metadata": {
                        "prompt_tokens": event.total_prompt_tokens,
                        "completion_tokens": event.total_completion_tokens,
                        "total_tokens":
                            event.total_prompt_tokens + event.total_completion_tokens,
                    },
                }),
            );
        }
    }

    /// POST a new LLM child run.  The UUID is derived deterministically from
    /// a per-run monotonic model invocation counter persisted in AgentState so
    /// repeated model calls across resume still get distinct spans.
    fn on_model_start(&self, event: &ModelStartTrace) -> impl Future<Output = ()> {
        let llm_id = Self::llm_run_id(&event.run_id, event.call_index);
        self.post(LangSmithRun {
            id: llm_id,
            parent_run_id: Some(event.run_id.0.clone()),
            run_type: "llm".to_string(),
            name: event.model.clone(),
            inputs: serde_json::json!({
                "messages": event.messages,
                "tools": event.tools,
            }),
            outputs: None,
            start_time: event.timestamp.to_rfc3339(),
            end_time: None,
            extra: None,
            session_name: self.project_name.clone(),
            status: None,
            error: None,
            metadata: None,
            usage_metadata: None,
        });
        async {}
    }

    /// PATCH the LLM child run with its outputs and token usage.
    fn on_model_end(&self, event: &ModelEndTrace) -> impl Future<Output = ()> {
        let llm_id = Self::llm_run_id(&event.run_id, event.call_index);
        let response_text = event.response_text.clone().unwrap_or_default();
        let finish_reason = if event.tool_calls.is_empty() {
            "stop"
        } else {
            "tool_calls"
        };
        let output_tool_calls = Self::llm_output_tool_calls(&event.tool_calls);
        let assistant_message = serde_json::json!({
            "role": "assistant",
            "content": response_text.clone(),
            "tool_calls": output_tool_calls.clone(),
        });
        let generations_message = assistant_message.clone();
        let output_message = assistant_message.clone();
        let messages = vec![assistant_message.clone()];
        self.patch(
            llm_id,
            serde_json::json!({
                "outputs": {
                    "response": event.response_text.clone(),
                    "text": response_text.clone(),
                    "output": output_message,
                    "messages": messages,
                    "tool_calls": output_tool_calls.clone(),
                    "message": assistant_message,
                    "generations": [{
                        "text": response_text,
                        "message": generations_message,
                        "generation_info": {
                            "finish_reason": finish_reason,
                        },
                    }],
                },
                "end_time": event.timestamp.to_rfc3339(),
                "status": "success",
                "usage_metadata": {
                    "prompt_tokens": event.prompt_tokens,
                    "completion_tokens": event.completion_tokens,
                    "total_tokens": event.prompt_tokens + event.completion_tokens,
                },
            }),
        );
        async {}
    }

    /// POST a new Tool child run.  The UUID is derived via UUID v5 from
    /// `run_id + tool_call_id` so that the raw OpenAI `call_abc123` format
    /// is converted to a valid UUID that LangSmith accepts.
    fn on_tool_start(&self, event: &ToolStartTrace) -> impl Future<Output = ()> {
        self.post_tool_run(
            &event.run_id,
            &event.tool_call_id,
            &event.tool_name,
            &event.arguments,
            event.timestamp,
        );
        async {}
    }

    /// PATCH the Tool child run with its result, status, and any error.
    fn on_tool_end(&self, event: &ToolEndTrace) -> impl Future<Output = ()> {
        self.patch_tool_run(
            &event.run_id,
            &event.tool_call_id,
            event.result.as_deref(),
            event.interrupted,
            event.error.as_deref(),
            event.timestamp,
        );
        async {}
    }

    /// PATCH the Chain Run to record interrupt details.
    ///
    /// The run is **not** ended here — `on_run_end(Interrupted)` follows
    /// shortly to update `status` and `end_time`.
    fn on_interrupt(&self, event: &InterruptTrace) -> impl Future<Output = ()> {
        async move {
            if !self.manage_root_run {
                return;
            }
            self.patch(
                event.run_id.0.clone(),
                serde_json::json!({
                    "extra": {
                        "interrupts": event.interrupts,
                        "interrupt_time": event.timestamp.to_rfc3339(),
                    },
                }),
            );
        }
    }

    /// PATCH the Chain Run to signal that execution is continuing.
    ///
    /// Clears `end_time` and `status` so LangSmith shows the run as
    /// still in-progress, and records `resume_time` in `extra`.
    fn on_resume(&self, event: &ResumeTrace) -> impl Future<Output = ()> {
        async move {
            if !self.manage_root_run {
                return;
            }
            self.patch(
                event.run_id.0.clone(),
                serde_json::json!({
                    "extra": {
                        "resume_time": event.timestamp.to_rfc3339(),
                        "resume_payloads_count": event.payloads_count,
                        "resume_outcomes": event.outcomes,
                    },
                    "end_time": serde_json::Value::Null,
                    "status": serde_json::Value::Null,
                }),
            );
        }
    }

    fn on_tool_execution_handoff(
        &self,
        event: &ToolExecutionHandoffTrace,
    ) -> impl Future<Output = ()> {
        for tool_call in &event.tool_calls {
            self.post_tool_run(
                &event.run_id,
                &tool_call.id,
                &tool_call.name,
                &tool_call.arguments,
                event.timestamp,
            );
        }
        async {}
    }

    fn on_external_tool_result(&self, event: &ExternalToolResultTrace) -> impl Future<Output = ()> {
        self.patch_tool_run(
            &event.run_id,
            &event.tool_call_id,
            event.result.as_deref(),
            false,
            event.error.as_deref(),
            event.timestamp,
        );
        async {}
    }

    /// No-op: turns are implicit in the LangSmith run tree via the sequence
    /// of LLM child runs.
    fn on_turn_start(&self, _event: &TurnStartTrace) -> impl Future<Output = ()> {
        async {}
    }

    fn on_custom(&self, name: &str, data: &serde_json::Value) -> impl Future<Output = ()> {
        let run_id = data
            .get("run_id")
            .and_then(|value| value.as_str())
            .map(|value| crate::types::RunId(value.to_string()));

        if let Some(run_id) = run_id {
            self.emit_custom_under(&run_id.0, name, data);
        }

        async {}
    }

    /// Flush currently queued tracing work without closing the tracer.
    fn on_flush(&self) -> impl Future<Output = ()> {
        LangSmithTracer::flush(self)
    }
}
