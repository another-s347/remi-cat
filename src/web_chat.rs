use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use bot_core::{
    todo::TodoItem, CatEvent, Content, ContextMetrics, ModelInputSnapshot, ReasoningEffort,
    SteerSubmitResult, ThreadHistoryMessage, TokenUsage, ToolApprovalDecision, ToolApprovalRequest,
    UserQuestionRequest, UserQuestionResponse,
};
use futures::StreamExt;
use remi_agentloop::prelude::CancellationToken;
use remi_agentloop::prelude::ProtocolEvent;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::session::{ChannelBinding, SubSessionKind};
use crate::{
    command::{process_runtime_commands, RuntimeCommandPipelineResult},
    model_input_store::upsert_model_input_snapshot_json,
    ChatChannel, ChatRequest, CoreChatEvent, Runtime, SESSION_AGENT_ID_METADATA_KEY,
    SESSION_MODEL_PROFILE_METADATA_KEY,
};

pub const WEB_CHANNEL: &str = "web";
pub const WEB_USER_ID: &str = "web-local";

#[derive(Clone)]
pub struct WebChatHandle {
    tx: mpsc::Sender<WebChatCommand>,
}

pub struct WebRun {
    pub events: mpsc::Receiver<ChatEventV1>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveWebRun {
    pub session_id: String,
    pub run_id: String,
    pub content: Content,
    pub started_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct WebRuntimeOptions {
    pub model_profile_id: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub agent_id: Option<String>,
    pub async_agent: Option<bool>,
}

pub(crate) enum WebChatCommand {
    Run {
        session_id: String,
        run_id: String,
        content: Content,
        runtime: WebRuntimeOptions,
        after_sequence: Option<u64>,
        response: oneshot::Sender<anyhow::Result<WebRun>>,
    },
    Steer {
        session_id: String,
        run_id: String,
        content: Content,
        response: oneshot::Sender<anyhow::Result<()>>,
    },
    Cancel {
        run_id: String,
        response: oneshot::Sender<bool>,
    },
    CancelSession {
        session_id: String,
        response: oneshot::Sender<bool>,
    },
    ActiveRun {
        session_id: String,
        response: oneshot::Sender<Option<ActiveWebRun>>,
    },
    ActiveRuns {
        response: oneshot::Sender<Vec<ActiveWebRun>>,
    },
    History {
        session_id: String,
        response: oneshot::Sender<Vec<ThreadHistoryMessage>>,
    },
    Events {
        session_id: String,
        after_sequence: Option<u64>,
        follow: bool,
        response: oneshot::Sender<anyhow::Result<WebRun>>,
    },
    Todos {
        session_id: String,
        response: oneshot::Sender<anyhow::Result<Vec<TodoItem>>>,
    },
    DeleteThread {
        session_id: String,
        response: oneshot::Sender<anyhow::Result<()>>,
    },
    ForkThread {
        source_session_id: String,
        target_session_id: String,
        user_id: Option<String>,
        response: oneshot::Sender<anyhow::Result<()>>,
    },
    UpdateSecretRedactor {
        entries: HashMap<String, String>,
        response: oneshot::Sender<()>,
    },
    DecideApproval {
        approval_id: String,
        decision: ToolApprovalDecision,
        response: oneshot::Sender<Option<ToolApprovalRequest>>,
    },
    AnswerUserQuestion {
        question_id: String,
        answer: UserQuestionResponse,
        response: oneshot::Sender<Option<UserQuestionRequest>>,
    },
}

struct ActiveRun {
    session_id: String,
    content: Content,
    started_at: String,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
    direct: mpsc::Sender<ChatEventV1>,
    log: Rc<RefCell<Vec<ChatEventV1>>>,
    broadcast: broadcast::Sender<ChatEventV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatEventV1 {
    pub version: u8,
    pub event: String,
    pub run_id: String,
    pub session_id: String,
    pub sequence: u64,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ChatEventV1 {
    fn new(
        event: impl Into<String>,
        run_id: &str,
        session_id: &str,
        sequence: u64,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            version: 1,
            event: event.into(),
            run_id: run_id.to_string(),
            session_id: session_id.to_string(),
            sequence,
            timestamp: chrono::Utc::now().to_rfc3339(),
            data,
        }
    }
}

impl WebChatHandle {
    pub fn channel() -> (Self, mpsc::Receiver<WebChatCommand>) {
        let (tx, rx) = mpsc::channel(64);
        (Self { tx }, rx)
    }

    pub async fn run(
        &self,
        session_id: String,
        run_id: String,
        content: Content,
        runtime: WebRuntimeOptions,
        after_sequence: Option<u64>,
    ) -> anyhow::Result<WebRun> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::Run {
                session_id,
                run_id,
                content,
                runtime,
                after_sequence,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped run response"))?
    }

    pub async fn steer(
        &self,
        session_id: String,
        run_id: String,
        content: Content,
    ) -> anyhow::Result<()> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::Steer {
                session_id,
                run_id,
                content,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped steer response"))?
    }

    pub async fn cancel(&self, run_id: String) -> anyhow::Result<bool> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::Cancel { run_id, response })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        Ok(rx.await.unwrap_or(false))
    }

    pub async fn cancel_session(&self, session_id: String) -> anyhow::Result<bool> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::CancelSession {
                session_id,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        Ok(rx.await.unwrap_or(false))
    }

    pub async fn active_run(&self, session_id: String) -> anyhow::Result<Option<ActiveWebRun>> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::ActiveRun {
                session_id,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped active run response"))
    }

    pub async fn active_runs(&self) -> anyhow::Result<Vec<ActiveWebRun>> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::ActiveRuns { response })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped active runs response"))
    }

    pub async fn history(&self, session_id: String) -> anyhow::Result<Vec<ThreadHistoryMessage>> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::History {
                session_id,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped history response"))
    }

    pub async fn events(
        &self,
        session_id: String,
        after_sequence: Option<u64>,
        follow: bool,
    ) -> anyhow::Result<WebRun> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::Events {
                session_id,
                after_sequence,
                follow,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped events response"))?
    }

    pub async fn todos(&self, session_id: String) -> anyhow::Result<Vec<TodoItem>> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::Todos {
                session_id,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped todo response"))?
    }

    pub async fn delete_thread(&self, session_id: String) -> anyhow::Result<()> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::DeleteThread {
                session_id,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped delete response"))?
    }

    pub async fn fork_thread(
        &self,
        source_session_id: String,
        target_session_id: String,
        user_id: Option<String>,
    ) -> anyhow::Result<()> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::ForkThread {
                source_session_id,
                target_session_id,
                user_id,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped fork response"))?
    }

    pub async fn update_secret_redactor(
        &self,
        entries: HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::UpdateSecretRedactor { entries, response })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped secret redactor response"))?;
        Ok(())
    }

    pub async fn decide_approval(
        &self,
        approval_id: String,
        decision: ToolApprovalDecision,
    ) -> anyhow::Result<Option<ToolApprovalRequest>> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::DecideApproval {
                approval_id,
                decision,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped approval response"))
    }

    pub async fn answer_user_question(
        &self,
        question_id: String,
        answer: UserQuestionResponse,
    ) -> anyhow::Result<Option<UserQuestionRequest>> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::AnswerUserQuestion {
                question_id,
                answer,
                response,
            })
            .await
            .map_err(|_| anyhow::anyhow!("web chat runtime is unavailable"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("web chat runtime dropped user question response"))
    }
}

pub async fn run_dispatcher(runtime: Rc<Runtime>, mut rx: mpsc::Receiver<WebChatCommand>) {
    let active: Rc<RefCell<HashMap<String, ActiveRun>>> = Rc::new(RefCell::new(HashMap::new()));
    let event_store = Rc::new(SessionEventStore::new(&runtime.data_dir));

    while let Some(command) = rx.recv().await {
        match command {
            WebChatCommand::Run {
                session_id,
                run_id,
                content,
                runtime: runtime_options,
                after_sequence,
                response,
            } => {
                if let Err(error) = runtime.bot.validate_runtime_overrides(
                    runtime_options.model_profile_id.as_deref(),
                    runtime_options.agent_id.as_deref(),
                    runtime_options.reasoning_effort,
                ) {
                    let _ =
                        response.send(Err(anyhow::anyhow!("invalid runtime override: {error}")));
                    continue;
                }
                if let Some(existing_run_id) =
                    active_run_id_for_session(&active.borrow(), &session_id)
                {
                    if existing_run_id == run_id {
                        let run = attach_active_run(&active.borrow(), &run_id, after_sequence);
                        let _ = response.send(run.ok_or_else(|| {
                            anyhow::anyhow!("active run disappeared while attaching")
                        }));
                        continue;
                    }
                    let _ = response.send(Err(anyhow::anyhow!(
                        "a run is already active for this session"
                    )));
                    continue;
                }
                let cancel = CancellationToken::new();
                let log = Rc::new(RefCell::new(Vec::new()));
                let (events_tx, events_rx) = mpsc::channel(128);
                let sink_direct = events_tx.clone();
                let (broadcast_tx, _) = broadcast::channel(512);
                let started_at = chrono::Utc::now().to_rfc3339();
                let _ = response.send(Ok(WebRun { events: events_rx }));

                let runtime = Rc::clone(&runtime);
                let active_for_task = Rc::clone(&active);
                let session_id_for_task = session_id.clone();
                let run_id_for_task = run_id.clone();
                let content_for_task = content.clone();
                let runtime_options_for_task = runtime_options.clone();
                let log_for_task = Rc::clone(&log);
                let broadcast_for_task = broadcast_tx.clone();
                let event_store_for_task = Rc::clone(&event_store);
                let cancel_for_task = cancel.clone();
                let handle = tokio::task::spawn_local(async move {
                    let sink = WebRunEventSink {
                        direct: events_tx,
                        log: log_for_task,
                        broadcast: broadcast_for_task,
                        event_store: event_store_for_task,
                    };
                    run_turn(
                        runtime,
                        &session_id_for_task,
                        &run_id_for_task,
                        content_for_task,
                        runtime_options_for_task,
                        cancel_for_task,
                        sink,
                    )
                    .await;
                    active_for_task.borrow_mut().remove(&run_id_for_task);
                });
                active.borrow_mut().insert(
                    run_id.clone(),
                    ActiveRun {
                        session_id: session_id.clone(),
                        content: content.clone(),
                        started_at,
                        cancel: cancel.clone(),
                        handle,
                        direct: sink_direct,
                        log: Rc::clone(&log),
                        broadcast: broadcast_tx.clone(),
                    },
                );
            }
            WebChatCommand::Steer {
                session_id,
                run_id,
                content,
                response,
            } => {
                let run_matches_session = active
                    .borrow()
                    .get(&run_id)
                    .is_some_and(|run| run.session_id == session_id);
                let child_thread_id = if run_matches_session {
                    None
                } else {
                    runtime
                        .sessions
                        .lock()
                        .await
                        .metadata_string(&session_id, "sub_session_thread_id")
                };
                if !run_matches_session && child_thread_id.is_none() {
                    let _ = response.send(Err(anyhow::anyhow!(
                        "no matching active run for this session"
                    )));
                    continue;
                }
                let text = content.text_content();
                if text.trim_start().starts_with('/') {
                    match process_runtime_commands(&runtime, &session_id, text.trim()).await {
                        Ok(RuntimeCommandPipelineResult::Reply(reply)) => {
                            if let Some(run) = active.borrow().get(&run_id) {
                                let sequence = run.log.borrow().len() as u64;
                                let web_event = ChatEventV1::new(
                                    "text_delta",
                                    &run_id,
                                    &session_id,
                                    sequence,
                                    Some(serde_json::json!({"text": reply})),
                                );
                                if let Ok(web_event) = event_store.append(web_event) {
                                    run.log.borrow_mut().push(web_event.clone());
                                    let _ = run.broadcast.send(web_event.clone());
                                    let _ = run.direct.try_send(web_event);
                                }
                            }
                            let _ = response.send(Ok(()));
                        }
                        Ok(RuntimeCommandPipelineResult::StartWorkflow { .. })
                        | Ok(RuntimeCommandPipelineResult::Continue { .. }) => {
                            let _ = response.send(Err(anyhow::anyhow!(
                                "command requires starting a new run; wait for the active run to finish"
                            )));
                        }
                        Err(error) => {
                            let _ = response.send(Err(error));
                        }
                    }
                    continue;
                }
                let request = ChatRequest::text(session_id.clone(), ChatChannel::Web, "")
                    .with_content(content)
                    .with_sender(WEB_USER_ID, Some(WEB_USER_ID.to_string()))
                    .with_message(format!("web-steer-{}", uuid::Uuid::new_v4()), "p2p")
                    .with_platform(Some(WEB_CHANNEL.to_string()));
                let result = if let Some(thread_id) = child_thread_id.as_deref() {
                    runtime
                        .bot
                        .submit_subagent_steer(&thread_id, request.into())
                } else {
                    runtime.submit_steer(request)
                };
                match result {
                    SteerSubmitResult::Queued(event) => {
                        if let Some(run) = active.borrow().get(&run_id) {
                            let sequence = run.log.borrow().len() as u64;
                            let web_event = ChatEventV1::new(
                                "steer_queued",
                                &run_id,
                                &session_id,
                                sequence,
                                Some(
                                    serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
                                ),
                            );
                            if let Ok(web_event) = event_store.append(web_event) {
                                run.log.borrow_mut().push(web_event.clone());
                                let _ = run.broadcast.send(web_event.clone());
                                let _ = run.direct.try_send(web_event);
                            }
                        } else {
                            let web_event = ChatEventV1::new(
                                "steer_queued",
                                &run_id,
                                &session_id,
                                0,
                                Some(
                                    serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
                                ),
                            );
                            let _ = event_store.append(web_event);
                        }
                        let _ = response.send(Ok(()));
                    }
                    SteerSubmitResult::NotRunning => {
                        let _ = response.send(Err(anyhow::anyhow!(
                            "active run ended before steer could be queued"
                        )));
                    }
                }
            }
            WebChatCommand::Cancel { run_id, response } => {
                let cancelled = active.borrow_mut().remove(&run_id).map(|run| {
                    run.cancel.cancel();
                    let cancelled_event = ChatEventV1::new(
                        "cancelled",
                        &run_id,
                        &run.session_id,
                        0,
                        Some(serde_json::json!({})),
                    );
                    if let Ok(event) = event_store.append(cancelled_event) {
                        let _ = run.broadcast.send(event.clone());
                        let _ = run.direct.try_send(event);
                    }
                    let event = ChatEventV1::new(
                        "run_finished",
                        &run_id,
                        &run.session_id,
                        0,
                        Some(serde_json::json!({"status": "cancelled"})),
                    );
                    if let Ok(event) = event_store.append(event) {
                        let _ = run.broadcast.send(event.clone());
                        let _ = run.direct.try_send(event);
                    }
                    let runtime = Rc::clone(&runtime);
                    let session_id = run.session_id.clone();
                    tokio::task::spawn_local(async move {
                        let _ = runtime.bot.cancel_background_tasks(&session_id).await;
                    });
                    run.handle.abort();
                    true
                });
                let _ = response.send(cancelled.unwrap_or(false));
            }
            WebChatCommand::CancelSession {
                session_id,
                response,
            } => {
                let mut cancelled = false;
                let run_ids = active
                    .borrow()
                    .iter()
                    .filter(|(_, run)| run.session_id == session_id)
                    .map(|(run_id, _)| run_id.clone())
                    .collect::<Vec<_>>();
                for run_id in run_ids {
                    if let Some(run) = active.borrow_mut().remove(&run_id) {
                        run.cancel.cancel();
                        let cancelled_event = ChatEventV1::new(
                            "cancelled",
                            &run_id,
                            &run.session_id,
                            0,
                            Some(serde_json::json!({})),
                        );
                        if let Ok(event) = event_store.append(cancelled_event) {
                            let _ = run.broadcast.send(event.clone());
                            let _ = run.direct.try_send(event);
                        }
                        let event = ChatEventV1::new(
                            "run_finished",
                            &run_id,
                            &run.session_id,
                            0,
                            Some(serde_json::json!({"status": "cancelled"})),
                        );
                        if let Ok(event) = event_store.append(event) {
                            let _ = run.broadcast.send(event.clone());
                            let _ = run.direct.try_send(event);
                        }
                        let runtime = Rc::clone(&runtime);
                        let session_id = run.session_id.clone();
                        tokio::task::spawn_local(async move {
                            let _ = runtime.bot.cancel_background_tasks(&session_id).await;
                        });
                        run.handle.abort();
                        cancelled = true;
                    }
                }
                if !cancelled {
                    let child_thread_id = runtime
                        .sessions
                        .lock()
                        .await
                        .metadata_string(&session_id, "sub_session_thread_id");
                    if let Some(thread_id) = child_thread_id {
                        cancelled = runtime.bot.cancel_subagent(&thread_id);
                        let _ = runtime.bot.cancel_background_tasks(&thread_id).await;
                        if cancelled {
                            let child_run_id = event_store
                                .read(&session_id)
                                .ok()
                                .and_then(|events| events.last().map(|event| event.run_id.clone()))
                                .unwrap_or_default();
                            let _ = event_store.append(ChatEventV1::new(
                                "cancelled",
                                &child_run_id,
                                &session_id,
                                0,
                                Some(serde_json::json!({})),
                            ));
                            let _ = event_store.append(ChatEventV1::new(
                                "run_finished",
                                &child_run_id,
                                &session_id,
                                0,
                                Some(serde_json::json!({"status": "cancelled"})),
                            ));
                        }
                    } else {
                        let _ = runtime.bot.cancel_background_tasks(&session_id).await;
                    }
                }
                let _ = response.send(cancelled);
            }
            WebChatCommand::ActiveRun {
                session_id,
                response,
            } => {
                let active_run = active
                    .borrow()
                    .iter()
                    .find(|(_, run)| run.session_id == session_id)
                    .map(|(run_id, run)| ActiveWebRun {
                        session_id: run.session_id.clone(),
                        run_id: run_id.clone(),
                        content: run.content.clone(),
                        started_at: run.started_at.clone(),
                    });
                let _ = response.send(active_run);
            }
            WebChatCommand::ActiveRuns { response } => {
                let active_runs = active
                    .borrow()
                    .iter()
                    .map(|(run_id, run)| ActiveWebRun {
                        session_id: run.session_id.clone(),
                        run_id: run_id.clone(),
                        content: run.content.clone(),
                        started_at: run.started_at.clone(),
                    })
                    .collect();
                let _ = response.send(active_runs);
            }
            WebChatCommand::History {
                session_id,
                response,
            } => {
                let thread_id = runtime
                    .sessions
                    .lock()
                    .await
                    .metadata_string(&session_id, "sub_session_thread_id")
                    .unwrap_or_else(|| session_id.clone());
                let mut history = runtime.bot.thread_history(&thread_id).await;
                for message in &mut history {
                    message.pretty = None;
                }
                if let Some(instance) = runtime.bot.workflow_status(&thread_id).await {
                    append_supervisor_history(&mut history, &session_id, &instance);
                }
                let _ = response.send(history);
            }
            WebChatCommand::Events {
                session_id,
                after_sequence,
                follow,
                response,
            } => {
                let _ = response.send(event_store.stream(&session_id, after_sequence, follow));
            }
            WebChatCommand::Todos {
                session_id,
                response,
            } => {
                let todos = runtime
                    .bot
                    .thread_todos(&session_id)
                    .await
                    .map_err(anyhow::Error::from);
                let _ = response.send(todos);
            }
            WebChatCommand::DeleteThread {
                session_id,
                response,
            } => {
                let run_ids = active
                    .borrow()
                    .iter()
                    .filter(|(_, run)| run.session_id == session_id)
                    .map(|(run_id, _)| run_id.clone())
                    .collect::<Vec<_>>();
                for run_id in run_ids {
                    if let Some(run) = active.borrow_mut().remove(&run_id) {
                        run.cancel.cancel();
                        run.handle.abort();
                    }
                }
                let thread_id = runtime
                    .sessions
                    .lock()
                    .await
                    .metadata_string(&session_id, "sub_session_thread_id")
                    .unwrap_or_else(|| session_id.clone());
                let result = runtime
                    .bot
                    .delete_thread_data(&thread_id, Some(WEB_USER_ID))
                    .await
                    .map_err(anyhow::Error::from);
                let result = result.and_then(|_| event_store.delete(&session_id));
                let _ = response.send(result);
            }
            WebChatCommand::ForkThread {
                source_session_id,
                target_session_id,
                user_id,
                response,
            } => {
                let result = runtime
                    .bot
                    .fork_thread_data(&source_session_id, &target_session_id, user_id.as_deref())
                    .await
                    .map_err(anyhow::Error::from);
                let _ = response.send(result);
            }
            WebChatCommand::UpdateSecretRedactor { entries, response } => {
                runtime.bot.update_secret_redactor(&entries);
                let _ = response.send(());
            }
            WebChatCommand::DecideApproval {
                approval_id,
                decision,
                response,
            } => {
                let request = runtime
                    .bot
                    .approval_manager()
                    .decide(&approval_id, decision)
                    .await;
                tracing::info!(
                    approval_id = %approval_id,
                    decision = ?decision,
                    source = "web_chat",
                    decided = request.is_some(),
                    tool_name = request.as_ref().map(|request| request.tool_name.as_str()).unwrap_or(""),
                    session_id = request.as_ref().map(|request| request.session_id.as_str()).unwrap_or(""),
                    "tool_approval.decision"
                );
                let _ = response.send(request);
            }
            WebChatCommand::AnswerUserQuestion {
                question_id,
                answer,
                response,
            } => {
                let request = runtime.bot.answer_user_question(&question_id, answer).await;
                tracing::info!(
                    question_id = %question_id,
                    source = "web_chat",
                    answered = request.is_some(),
                    session_id = request.as_ref().map(|request| request.session_id.as_str()).unwrap_or(""),
                    "user_question.decision"
                );
                let _ = response.send(request);
            }
        }
    }
}

fn active_run_id_for_session(
    active: &HashMap<String, ActiveRun>,
    session_id: &str,
) -> Option<String> {
    active
        .iter()
        .find(|(_, run)| run.session_id == session_id)
        .map(|(run_id, _)| run_id.clone())
}

fn attach_active_run(
    active: &HashMap<String, ActiveRun>,
    run_id: &str,
    after_sequence: Option<u64>,
) -> Option<WebRun> {
    let run = active.get(run_id)?;
    let (tx, rx) = mpsc::channel(128);
    let mut broadcast_rx = run.broadcast.subscribe();
    let replay = run
        .log
        .borrow()
        .iter()
        .filter(|event| after_sequence.is_none_or(|sequence| event.sequence > sequence))
        .cloned()
        .collect::<Vec<_>>();
    let replay_last = replay.last().map(|event| event.sequence).or(after_sequence);
    tokio::task::spawn_local(async move {
        for event in replay {
            if tx.send(event).await.is_err() {
                return;
            }
        }
        loop {
            match broadcast_rx.recv().await {
                Ok(event) if replay_last.is_none_or(|sequence| event.sequence > sequence) => {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Some(WebRun { events: rx })
}

fn append_supervisor_history(
    history: &mut Vec<ThreadHistoryMessage>,
    session_id: &str,
    instance: &bot_core::WorkflowInstance,
) {
    let report = instance
        .last_report
        .clone()
        .unwrap_or_else(|| workflow_status_report(instance));
    let call_id = format!("{session_id}-supervisor-status");
    let assistant_id = format!("{call_id}-assistant");
    let tool_id = format!("{call_id}-tool");
    let timestamp = Some(instance.updated_at.to_rfc3339());
    let events = serde_json::to_value(&report.supervisor_trace)
        .unwrap_or_else(|_| serde_json::Value::Array(Vec::new()));
    let arguments = serde_json::json!({ "events": events }).to_string();
    history.push(ThreadHistoryMessage {
        id: assistant_id,
        role: "assistant".to_string(),
        text: String::new(),
        timestamp: timestamp.clone(),
        tool_call_id: None,
        tool_calls: Some(serde_json::json!([{
            "id": call_id,
            "type": "function",
            "function": {
                "name": "__remi_supervisor",
                "arguments": arguments,
            }
        }])),
        pretty: None,
    });
    history.push(ThreadHistoryMessage {
        id: tool_id,
        role: "tool".to_string(),
        text: serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_string()),
        timestamp,
        tool_call_id: Some(call_id),
        tool_calls: None,
        pretty: None,
    });
}

fn workflow_status_report(instance: &bot_core::WorkflowInstance) -> bot_core::WorkflowReport {
    bot_core::WorkflowReport {
        workflow_id: instance.definition.id.clone(),
        workflow_name: instance.definition.name.clone(),
        from_node: instance.current_node.clone(),
        edge: instance.incoming_edge.clone(),
        to_node: instance.current_node.clone(),
        path_edges: Vec::new(),
        path_nodes: Vec::new(),
        status: instance.status.clone(),
        reason: match instance.status {
            bot_core::WorkflowStatus::Active => "workflow is active".to_string(),
            bot_core::WorkflowStatus::Paused => "workflow is paused".to_string(),
            bot_core::WorkflowStatus::Completed => "workflow is completed".to_string(),
            bot_core::WorkflowStatus::Stopped => "workflow is stopped".to_string(),
            bot_core::WorkflowStatus::Error => "workflow is in error state".to_string(),
        },
        agent_message: None,
        next_node_message: instance.node_message.clone(),
        supervisor_trace: Vec::new(),
        round: 0,
        max_rounds: instance.max_rounds.clone(),
        error: None,
    }
}

#[derive(Clone)]
struct WebRunEventSink {
    direct: mpsc::Sender<ChatEventV1>,
    log: Rc<RefCell<Vec<ChatEventV1>>>,
    broadcast: broadcast::Sender<ChatEventV1>,
    event_store: Rc<SessionEventStore>,
}

async fn send_event(sink: &WebRunEventSink, event: ChatEventV1) {
    let event = match sink.event_store.append(event.clone()) {
        Ok(event) => event,
        Err(error) => {
            tracing::error!(%error, "failed to persist web session event");
            event
        }
    };
    sink.log.borrow_mut().push(event.clone());
    let _ = sink.broadcast.send(event.clone());
    let _ = sink.direct.send(event).await;
}

struct SessionEventStore {
    root: PathBuf,
    next_sequences: RefCell<HashMap<String, u64>>,
    broadcasts: RefCell<HashMap<String, broadcast::Sender<ChatEventV1>>>,
    files: RefCell<HashMap<String, std::fs::File>>,
}

impl SessionEventStore {
    fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("session-events"),
            next_sequences: RefCell::new(HashMap::new()),
            broadcasts: RefCell::new(HashMap::new()),
            files: RefCell::new(HashMap::new()),
        }
    }

    fn path(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{session_id}.jsonl"))
    }

    fn read(&self, session_id: &str) -> anyhow::Result<Vec<ChatEventV1>> {
        let path = self.path(session_id);
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut events = Vec::new();
        for line in std::io::BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(&line) {
                Ok(event) => events.push(event),
                Err(error) => {
                    tracing::warn!(session_id, %error, "ignoring incomplete session event tail");
                    break;
                }
            }
        }
        Ok(events)
    }

    fn sender(&self, session_id: &str) -> broadcast::Sender<ChatEventV1> {
        self.broadcasts
            .borrow_mut()
            .entry(session_id.to_string())
            .or_insert_with(|| broadcast::channel(1024).0)
            .clone()
    }

    fn append(&self, mut event: ChatEventV1) -> anyhow::Result<ChatEventV1> {
        std::fs::create_dir_all(&self.root)?;
        let next = if let Some(next) = self.next_sequences.borrow().get(&event.session_id).copied()
        {
            next
        } else {
            self.read(&event.session_id)?
                .last()
                .map(|event| event.sequence.saturating_add(1))
                .unwrap_or(0)
        };
        event.sequence = next;
        let mut files = self.files.borrow_mut();
        if !files.contains_key(&event.session_id) {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.path(&event.session_id))?;
            files.insert(event.session_id.clone(), file);
        }
        let file = files
            .get_mut(&event.session_id)
            .expect("event file inserted above");
        serde_json::to_writer(&mut *file, &event)?;
        file.write_all(b"\n")?;
        file.flush()?;
        if event.event == "run_finished" {
            file.sync_data()?;
        }
        self.next_sequences
            .borrow_mut()
            .insert(event.session_id.clone(), next.saturating_add(1));
        let _ = self.sender(&event.session_id).send(event.clone());
        Ok(event)
    }

    fn stream(
        &self,
        session_id: &str,
        after_sequence: Option<u64>,
        follow: bool,
    ) -> anyhow::Result<WebRun> {
        let mut live = self.sender(session_id).subscribe();
        let replay = self
            .read(session_id)?
            .into_iter()
            .filter(|event| after_sequence.is_none_or(|sequence| event.sequence > sequence))
            .collect::<Vec<_>>();
        let last_replayed = replay.last().map(|event| event.sequence).or(after_sequence);
        let (tx, rx) = mpsc::channel(128);
        tokio::task::spawn_local(async move {
            for event in replay {
                if tx.send(event).await.is_err() {
                    return;
                }
            }
            if !follow {
                return;
            }
            loop {
                match live.recv().await {
                    Ok(event) if last_replayed.is_none_or(|sequence| event.sequence > sequence) => {
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(WebRun { events: rx })
    }

    fn delete(&self, session_id: &str) -> anyhow::Result<()> {
        self.next_sequences.borrow_mut().remove(session_id);
        self.broadcasts.borrow_mut().remove(session_id);
        self.files.borrow_mut().remove(session_id);
        match std::fs::remove_file(self.path(session_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn persist_model_input_snapshot(
    runtime: &Runtime,
    session_id: &str,
    snapshot: &ModelInputSnapshot,
) {
    let snapshot_json = match serde_json::to_string(snapshot) {
        Ok(json) => json,
        Err(err) => {
            tracing::warn!(
                session_id,
                run_id = %snapshot.run_id,
                error = %err,
                "failed to serialize model input snapshot"
            );
            return;
        }
    };
    if let Err(err) = upsert_model_input_snapshot_json(
        &runtime.data_dir,
        session_id,
        &snapshot.run_id,
        &snapshot_json,
    ) {
        tracing::warn!(
            session_id,
            run_id = %snapshot.run_id,
            error = %err,
            "failed to persist model input snapshot"
        );
    }
}

async fn ensure_web_sub_session(
    runtime: &Runtime,
    parent_session_id: &str,
    event: &remi_agentloop::prelude::SubSessionEvent,
) -> anyhow::Result<String> {
    let kind = if event.agent_name == "acp" {
        SubSessionKind::Acp
    } else {
        SubSessionKind::Agent
    };
    let status = sub_session_status(event.event.as_ref());
    {
        let mut sessions = runtime.sessions.lock().await;
        sessions.upsert_sub_session(
            parent_session_id,
            &event.sub_thread_id.0,
            kind,
            &event.agent_name,
            event.title.clone(),
            status,
        )?;
    }
    let channel_id = format!("sub-session:{}", event.sub_thread_id.0);
    let child = runtime.sessions.lock().await.create_channel(
        WEB_CHANNEL,
        &channel_id,
        &runtime.root_agent_id,
        event.title.clone(),
    )?;
    let mut sessions = runtime.sessions.lock().await;
    sessions.bind_sub_session_channel(
        parent_session_id,
        &event.sub_thread_id.0,
        ChannelBinding {
            platform: WEB_CHANNEL.to_string(),
            channel_id,
        },
    )?;
    sessions.set_metadata_values(
        &child.id,
        [
            (
                "sub_session_parent_session_id".to_string(),
                serde_json::Value::String(parent_session_id.to_string()),
            ),
            (
                "sub_session_thread_id".to_string(),
                serde_json::Value::String(event.sub_thread_id.0.clone()),
            ),
            (
                "sub_session_agent".to_string(),
                serde_json::Value::String(event.agent_name.clone()),
            ),
            (
                SESSION_AGENT_ID_METADATA_KEY.to_string(),
                serde_json::Value::String(event.agent_name.clone()),
            ),
        ],
    )?;
    Ok(child.id)
}

fn persist_sub_session_event(
    store: &SessionEventStore,
    session_id: &str,
    event: &remi_agentloop::prelude::SubSessionEvent,
) {
    let (kind, data) = protocol_event_payload(event.event.as_ref());
    let web_event = ChatEventV1::new(kind, &event.sub_run_id.0, session_id, 0, Some(data));
    if let Err(error) = store.append(web_event) {
        tracing::warn!(session_id, %error, "failed to persist sub-session event");
    }
}

fn protocol_event_payload(event: &ProtocolEvent) -> (&'static str, serde_json::Value) {
    match event {
        ProtocolEvent::RunStart { .. } => (
            "run_started",
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
        ProtocolEvent::Delta { content, role } => (
            "text_delta",
            serde_json::json!({"text": content, "role": role}),
        ),
        ProtocolEvent::ThinkingStart => ("thinking_started", serde_json::json!({})),
        ProtocolEvent::ThinkingDelta { content } => {
            ("thinking_delta", serde_json::json!({"text": content}))
        }
        ProtocolEvent::ThinkingEnd { content } => {
            ("thinking_finished", serde_json::json!({"text": content}))
        }
        ProtocolEvent::ToolCallStart { id, name } => (
            "tool_call_started",
            serde_json::json!({"call_id": id, "tool_name": name}),
        ),
        ProtocolEvent::ToolCallDelta {
            id,
            arguments_delta,
        } => (
            "tool_call_arguments_delta",
            serde_json::json!({"call_id": id, "delta": arguments_delta}),
        ),
        ProtocolEvent::ToolDelta { id, name, delta } => (
            "tool_delta",
            serde_json::json!({"call_id": id, "tool_name": name, "delta": delta}),
        ),
        ProtocolEvent::ToolResult { id, name, result } => (
            "tool_call_result",
            serde_json::json!({
                "call_id": id,
                "tool_name": name,
                "result": result,
                "success": bot_core::tool_success(result),
            }),
        ),
        ProtocolEvent::Usage {
            prompt_tokens,
            completion_tokens,
        } => (
            "stats",
            serde_json::json!({
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
            }),
        ),
        ProtocolEvent::Error { message, code } => (
            "error",
            serde_json::json!({"message": message, "code": code}),
        ),
        ProtocolEvent::Done => ("run_finished", serde_json::json!({"status": "completed"})),
        ProtocolEvent::Cancelled => ("run_finished", serde_json::json!({"status": "cancelled"})),
        ProtocolEvent::Custom { event_type, extra } => (
            "custom",
            serde_json::json!({"event_type": event_type, "data": extra}),
        ),
        _ => (
            "protocol_event",
            serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
        ),
    }
}

fn stats_payload(
    usage: TokenUsage,
    context_tokens: u32,
    first_response_at: Option<Duration>,
    model_elapsed_ms: u64,
    elapsed_ms: u128,
) -> serde_json::Value {
    let context = ContextMetrics::from_usage(usage, context_tokens);
    serde_json::json!({
        "ttft_ms": first_response_at.map(|elapsed| elapsed.as_millis() as u64),
        "prompt_tokens": usage.prompt_tokens,
        "completion_tokens": usage.completion_tokens,
        "total_tokens": usage.total_tokens(),
        "max_prompt_tokens": usage.max_prompt_tokens,
        "context_tokens": context.context_tokens,
        "context_usage": context.ratio,
        "model_elapsed_ms": model_elapsed_ms,
        "elapsed_ms": elapsed_ms,
    })
}

enum WebEventMap {
    Emit(&'static str, serde_json::Value),
    Finish(&'static str),
    Skip,
}

fn sub_session_status(event: &ProtocolEvent) -> &'static str {
    match event {
        ProtocolEvent::Done => "completed",
        ProtocolEvent::Cancelled => "cancelled",
        ProtocolEvent::Error { .. } => "error",
        ProtocolEvent::Custom { event_type, .. } if event_type == "sub_session_done" => "completed",
        ProtocolEvent::Custom { event_type, .. } if event_type == "sub_session_handed_off" => {
            "handed_off"
        }
        _ => "running",
    }
}

struct WebCoreEventMapper {
    context_tokens: u32,
    run_started_at: Instant,
    first_response_at: Option<Duration>,
    total_prompt_tokens: u32,
    total_completion_tokens: u32,
    max_prompt_tokens: u32,
    model_elapsed_ms: u64,
    model_input_snapshot: Option<ModelInputSnapshot>,
    terminal_status: &'static str,
}

impl WebCoreEventMapper {
    fn new(context_tokens: u32, run_started_at: Instant) -> Self {
        Self {
            context_tokens,
            run_started_at,
            first_response_at: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            max_prompt_tokens: 0,
            model_elapsed_ms: 0,
            model_input_snapshot: None,
            terminal_status: "completed",
        }
    }

    fn usage(&self) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.total_prompt_tokens,
            completion_tokens: self.total_completion_tokens,
            max_prompt_tokens: self.max_prompt_tokens,
        }
    }

    fn stats_payload(&self) -> serde_json::Value {
        stats_payload(
            self.usage(),
            self.context_tokens,
            self.first_response_at,
            self.model_elapsed_ms,
            self.run_started_at.elapsed().as_millis(),
        )
    }

    fn mark_first_response(&mut self) {
        self.first_response_at
            .get_or_insert_with(|| self.run_started_at.elapsed());
    }

    fn map(&mut self, runtime: &Runtime, session_id: &str, event: CoreChatEvent) -> WebEventMap {
        match event {
            CoreChatEvent::SupervisorStarted => WebEventMap::Skip,
            CoreChatEvent::Prefix(text) | CoreChatEvent::Reply(text) => {
                self.mark_first_response();
                WebEventMap::Emit("text_delta", serde_json::json!({"text": text}))
            }
            CoreChatEvent::Done => WebEventMap::Finish(self.terminal_status),
            CoreChatEvent::Bot(event) => self.map_cat_event(runtime, session_id, event),
        }
    }

    fn map_cat_event(
        &mut self,
        runtime: &Runtime,
        session_id: &str,
        event: CatEvent,
    ) -> WebEventMap {
        match event {
            CatEvent::Text(text) => {
                self.mark_first_response();
                WebEventMap::Emit("text_delta", serde_json::json!({"text": text}))
            }
            CatEvent::Thinking(text) => {
                self.mark_first_response();
                WebEventMap::Emit("thinking_delta", serde_json::json!({"text": text}))
            }
            CatEvent::ToolCallStart { id, name } => {
                self.mark_first_response();
                WebEventMap::Emit(
                    "tool_call_started",
                    serde_json::json!({
                        "call_id": id,
                        "tool_name": name,
                    }),
                )
            }
            CatEvent::ToolCallArgumentsDelta { id, delta } => WebEventMap::Emit(
                "tool_call_arguments_delta",
                serde_json::json!({"call_id": id, "delta": delta}),
            ),
            CatEvent::ToolCall { id, name, args } => {
                self.mark_first_response();
                WebEventMap::Emit(
                    "tool_call",
                    serde_json::json!({
                        "call_id": id,
                        "tool_name": name,
                        "args": args,
                    }),
                )
            }
            CatEvent::ToolCallResult {
                id,
                name,
                args,
                result,
                success,
                elapsed_ms,
            } => WebEventMap::Emit(
                "tool_call_result",
                serde_json::json!({
                    "call_id": id,
                    "tool_name": name,
                    "args": args,
                    "result": result,
                    "success": success,
                    "elapsed_ms": elapsed_ms,
                }),
            ),
            CatEvent::ToolApprovalRequested(request) => WebEventMap::Emit(
                "approval_requested",
                serde_json::to_value(request).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::ToolApprovalUpdated(request) => WebEventMap::Emit(
                "approval_updated",
                serde_json::to_value(request).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::ToolApprovalResolved { request, decision } => WebEventMap::Emit(
                "approval_resolved",
                serde_json::json!({
                    "request": request,
                    "decision": decision,
                }),
            ),
            CatEvent::UserQuestionRequested(request) => WebEventMap::Emit(
                "user_question_requested",
                serde_json::to_value(request).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::UserQuestionUpdated(request) => WebEventMap::Emit(
                "user_question_updated",
                serde_json::to_value(request).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::UserQuestionResolved { request, response } => WebEventMap::Emit(
                "user_question_resolved",
                serde_json::json!({
                    "request": request,
                    "response": response,
                }),
            ),
            // Registered asynchronously by `run_turn`, which mirrors the full
            // event into the child session and emits only the child link here.
            CatEvent::SubSession(_) => WebEventMap::Skip,
            CatEvent::SupervisorProgress(progress) => WebEventMap::Emit(
                "supervisor_progress",
                serde_json::to_value(progress).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::Supervisor(report) => WebEventMap::Emit(
                "supervisor_report",
                serde_json::to_value(report).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::ContextCompaction(event) => WebEventMap::Emit(
                "context_compaction",
                serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::StateUpdate(user_state) => WebEventMap::Emit(
                "todo_state",
                serde_json::json!({
                    "items": bot_core::todo::todos_from_user_state(&user_state),
                }),
            ),
            CatEvent::ModelInputSnapshot(snapshot) => {
                persist_model_input_snapshot(runtime, session_id, &snapshot);
                self.model_input_snapshot = Some(snapshot.clone());
                WebEventMap::Emit(
                    "model_input_snapshot",
                    serde_json::json!({
                        "run_id": snapshot.run_id,
                        "estimated_tokens": snapshot.totals.estimated_tokens,
                        "segment_count": snapshot.segments.len(),
                    }),
                )
            }
            CatEvent::SteerQueued(event) => WebEventMap::Emit(
                "steer_queued",
                serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::SteerInjected(event) => WebEventMap::Emit(
                "steer_injected",
                serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::Stats {
                prompt_tokens,
                completion_tokens,
                max_prompt_tokens,
                elapsed_ms,
            } => self.map_stats(
                runtime,
                session_id,
                prompt_tokens,
                completion_tokens,
                max_prompt_tokens,
                elapsed_ms,
            ),
            CatEvent::BackgroundTasksWaiting { count } => WebEventMap::Emit(
                "background_tasks_waiting",
                serde_json::json!({"count": count}),
            ),
            CatEvent::ToolTaskCompleted(task) => WebEventMap::Emit(
                "tool_task_completed",
                serde_json::to_value(task).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::Skill(event) => WebEventMap::Emit(
                "skill_changed",
                serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::Todo(event) => WebEventMap::Emit(
                "todo_changed",
                serde_json::to_value(event).unwrap_or(serde_json::Value::Null),
            ),
            CatEvent::Cancelled => {
                self.terminal_status = "cancelled";
                WebEventMap::Emit("cancelled", serde_json::json!({}))
            }
            CatEvent::UserInterrupted { reason } => {
                self.terminal_status = "interrupted";
                WebEventMap::Emit("user_interrupted", serde_json::json!({"reason": reason}))
            }
            CatEvent::Error(error) => {
                crate::telemetry::capture_agent_error(&error, "web.chat");
                self.terminal_status = "error";
                WebEventMap::Emit("error", serde_json::json!({"message": error.to_string()}))
            }
            CatEvent::Done => WebEventMap::Finish(self.terminal_status),
            CatEvent::History(_, _) => WebEventMap::Skip,
        }
    }

    fn map_stats(
        &mut self,
        runtime: &Runtime,
        session_id: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
        max_prompt_tokens: u32,
        elapsed_ms: u64,
    ) -> WebEventMap {
        self.total_prompt_tokens = prompt_tokens;
        self.total_completion_tokens = completion_tokens;
        self.max_prompt_tokens = max_prompt_tokens;
        self.model_elapsed_ms = elapsed_ms;
        let context_tokens = self.context_tokens;
        if let Some(snapshot) = &mut self.model_input_snapshot {
            snapshot.apply_usage(
                self.total_prompt_tokens,
                self.total_completion_tokens,
                self.max_prompt_tokens,
                context_tokens,
            );
            persist_model_input_snapshot(runtime, session_id, snapshot);
        }
        WebEventMap::Emit("stats", self.stats_payload())
    }

    #[cfg(test)]
    fn map_stats_without_snapshot(
        &mut self,
        prompt_tokens: u32,
        completion_tokens: u32,
        max_prompt_tokens: u32,
        elapsed_ms: u64,
    ) -> WebEventMap {
        self.total_prompt_tokens = prompt_tokens;
        self.total_completion_tokens = completion_tokens;
        self.max_prompt_tokens = max_prompt_tokens;
        self.model_elapsed_ms = elapsed_ms;
        WebEventMap::Emit("stats", self.stats_payload())
    }
}

async fn run_turn(
    runtime: Rc<Runtime>,
    session_id: &str,
    run_id: &str,
    content: Content,
    runtime_options: WebRuntimeOptions,
    cancel: CancellationToken,
    sink: WebRunEventSink,
) {
    let run_started_at = Instant::now();
    let mut sequence = 0;
    send_event(
        &sink,
        ChatEventV1::new("run_started", run_id, session_id, sequence, None),
    )
    .await;
    sequence += 1;

    let text = content.text_content();
    if is_web_fork_command(text.trim()) {
        match fork_web_session_command(&runtime, session_id, text.trim()).await {
            Ok(fork) => {
                send_event(
                    &sink,
                    ChatEventV1::new(
                        "text_delta",
                        run_id,
                        session_id,
                        sequence,
                        Some(serde_json::json!({
                            "text": format!(
                                "已 fork 当前 session。\n\n新 session: `{}`\n标题: {}",
                                fork.id,
                                fork.title.as_deref().unwrap_or("新对话"),
                            ),
                        })),
                    ),
                )
                .await;
                sequence += 1;
                send_event(
                    &sink,
                    ChatEventV1::new(
                        "session_forked",
                        run_id,
                        session_id,
                        sequence,
                        Some(serde_json::to_value(&fork).unwrap_or(serde_json::Value::Null)),
                    ),
                )
                .await;
                sequence += 1;
                send_event(
                    &sink,
                    ChatEventV1::new(
                        "run_finished",
                        run_id,
                        session_id,
                        sequence,
                        Some(serde_json::json!({"status": "completed"})),
                    ),
                )
                .await;
            }
            Err(error) => {
                send_event(
                    &sink,
                    ChatEventV1::new(
                        "error",
                        run_id,
                        session_id,
                        sequence,
                        Some(serde_json::json!({"message": error.to_string()})),
                    ),
                )
                .await;
                sequence += 1;
                send_event(
                    &sink,
                    ChatEventV1::new(
                        "run_finished",
                        run_id,
                        session_id,
                        sequence,
                        Some(serde_json::json!({"status": "error"})),
                    ),
                )
                .await;
            }
        }
        return;
    }

    let (model_profile_id, agent_id) = {
        let sessions = runtime.sessions.lock().await;
        (
            sessions.metadata_string(session_id, SESSION_MODEL_PROFILE_METADATA_KEY),
            sessions.metadata_string(session_id, SESSION_AGENT_ID_METADATA_KEY),
        )
    };
    let context_tokens = runtime.bot.model_context_tokens_for_agent(
        runtime_options
            .model_profile_id
            .as_deref()
            .or(model_profile_id.as_deref()),
        runtime_options.agent_id.as_deref().or(agent_id.as_deref()),
    );
    let request = ChatRequest::text(session_id.to_string(), ChatChannel::Web, "")
        .with_content(content)
        .with_runtime_overrides(
            runtime_options.model_profile_id,
            runtime_options.reasoning_effort,
            runtime_options.agent_id,
            runtime_options.async_agent,
        )
        .with_sender(WEB_USER_ID, Some(WEB_USER_ID.to_string()))
        .with_message(run_id.to_string(), "p2p")
        .with_platform(Some(WEB_CHANNEL.to_string()))
        .with_cancel(cancel);
    let mut stream = std::pin::pin!(Rc::clone(&runtime).chat(request));
    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);
    let mut event_mapper = WebCoreEventMapper::new(context_tokens, run_started_at);
    let mut final_status = "completed";
    let mut sub_sessions = HashMap::<String, (String, String)>::new();

    loop {
        let event = tokio::select! {
            event = stream.next() => event,
            _ = &mut timeout => {
                send_event(&sink, ChatEventV1::new(
                    "error", run_id, session_id, sequence,
                    Some(serde_json::json!({"message": "reply timed out"})),
                )).await;
                sequence += 1;
                final_status = "error";
                None
            }
        };
        let Some(event) = event else { break };
        if let CoreChatEvent::Bot(CatEvent::SubSession(sub_event)) = &event {
            let thread_id = sub_event.sub_thread_id.0.clone();
            let status = sub_session_status(sub_event.event.as_ref()).to_string();
            let previous = sub_sessions.get(&thread_id).cloned();
            let needs_update = previous
                .as_ref()
                .is_none_or(|(_, previous_status)| previous_status != &status);
            let child = if needs_update {
                ensure_web_sub_session(&runtime, session_id, sub_event)
                    .await
                    .map(|child_id| {
                        sub_sessions.insert(thread_id, (child_id.clone(), status.clone()));
                        child_id
                    })
            } else {
                Ok(previous.expect("checked above").0)
            };
            match child {
                Ok(child_session_id) => {
                    persist_sub_session_event(&sink.event_store, &child_session_id, sub_event);
                    if needs_update {
                        send_event(
                            &sink,
                            ChatEventV1::new(
                                "sub_session",
                                run_id,
                                session_id,
                                sequence,
                                Some(serde_json::json!({
                                    "session_id": child_session_id,
                                    "status": status,
                                    "parent_call_id": sub_event.parent_tool_call_id,
                                })),
                            ),
                        )
                        .await;
                        sequence += 1;
                    }
                }
                Err(error) => tracing::warn!(%error, "failed to register web sub-session"),
            }
            continue;
        }
        match event_mapper.map(&runtime, session_id, event) {
            WebEventMap::Emit(kind, data) => {
                send_event(
                    &sink,
                    ChatEventV1::new(kind, run_id, session_id, sequence, Some(data)),
                )
                .await;
                sequence += 1;
            }
            WebEventMap::Finish(status) => {
                final_status = status;
                break;
            }
            WebEventMap::Skip => {}
        }
    }
    if final_status != "error" {
        final_status = event_mapper.terminal_status;
    }

    send_event(
        &sink,
        ChatEventV1::new(
            "stats",
            run_id,
            session_id,
            sequence,
            Some(event_mapper.stats_payload()),
        ),
    )
    .await;
    sequence += 1;
    send_event(
        &sink,
        ChatEventV1::new(
            "run_finished",
            run_id,
            session_id,
            sequence,
            Some(serde_json::json!({"status": final_status})),
        ),
    )
    .await;
}

pub(crate) fn is_web_fork_command(command: &str) -> bool {
    command == "/fork" || command.starts_with("/fork ")
}

async fn fork_web_session_command(
    runtime: &Runtime,
    source_session_id: &str,
    command: &str,
) -> anyhow::Result<crate::session::Session> {
    if runtime.bot.is_thread_running(source_session_id).await {
        anyhow::bail!("当前 session 正在运行，结束或取消后再 fork。");
    }
    let title = command
        .strip_prefix("/fork")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let channel_id = uuid::Uuid::new_v4().to_string();
    let fork = runtime
        .sessions
        .lock()
        .await
        .fork_session(source_session_id, WEB_CHANNEL, &channel_id, title)?
        .ok_or_else(|| anyhow::anyhow!("source session `{source_session_id}` not found"))?;
    if let Err(error) = runtime
        .bot
        .fork_thread_data(source_session_id, &fork.id, Some(WEB_USER_ID))
        .await
    {
        let _ = runtime.sessions.lock().await.delete(&fork.id);
        return Err(anyhow::Error::from(error));
    }
    Ok(fork)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        protocol_event_payload, stats_payload, ChatEventV1, SessionEventStore, WebCoreEventMapper,
        WebEventMap,
    };
    use bot_core::TokenUsage;

    #[test]
    fn stats_payload_includes_context_usage_and_timings() {
        let payload = stats_payload(
            TokenUsage {
                prompt_tokens: 40,
                completion_tokens: 2,
                max_prompt_tokens: 100,
            },
            200,
            Some(Duration::from_millis(12)),
            34,
            56,
        );

        assert_eq!(payload["ttft_ms"], 12);
        assert_eq!(payload["prompt_tokens"], 40);
        assert_eq!(payload["completion_tokens"], 2);
        assert_eq!(payload["total_tokens"], 42);
        assert_eq!(payload["max_prompt_tokens"], 100);
        assert_eq!(payload["context_tokens"], 200);
        assert_eq!(payload["model_elapsed_ms"], 34);
        assert_eq!(payload["elapsed_ms"], 56);
    }

    #[test]
    fn web_mapper_tracks_stats_for_final_payload() {
        let mut mapper = WebCoreEventMapper::new(1000, Instant::now());

        let mapped = mapper.map_stats_without_snapshot(100, 25, 500, 77);

        match mapped {
            WebEventMap::Emit(kind, payload) => {
                assert_eq!(kind, "stats");
                assert_eq!(payload["prompt_tokens"], 100);
                assert_eq!(payload["completion_tokens"], 25);
                assert_eq!(payload["max_prompt_tokens"], 500);
                assert_eq!(payload["context_tokens"], 1000);
                assert_eq!(payload["model_elapsed_ms"], 77);
            }
            _ => panic!("expected stats event"),
        }
        let final_payload = mapper.stats_payload();
        assert_eq!(final_payload["prompt_tokens"], 100);
        assert_eq!(final_payload["completion_tokens"], 25);
    }

    #[test]
    fn session_event_store_persists_monotonic_sequences() {
        let data_dir = tempfile::tempdir().expect("tempdir");
        let store = SessionEventStore::new(data_dir.path());
        let first = store
            .append(ChatEventV1::new(
                "text_delta",
                "run-1",
                "session-1",
                99,
                None,
            ))
            .expect("first event");
        let second = store
            .append(ChatEventV1::new(
                "run_finished",
                "run-1",
                "session-1",
                0,
                None,
            ))
            .expect("second event");
        assert_eq!(first.sequence, 0);
        assert_eq!(second.sequence, 1);

        let reloaded = SessionEventStore::new(data_dir.path());
        let third = reloaded
            .append(ChatEventV1::new(
                "run_started",
                "run-2",
                "session-1",
                0,
                None,
            ))
            .expect("third event");
        assert_eq!(third.sequence, 2);
        assert_eq!(reloaded.read("session-1").expect("read").len(), 3);
    }

    #[test]
    fn sub_session_protocol_events_use_raw_web_shapes() {
        let event = remi_agentloop::prelude::ProtocolEvent::ToolCallDelta {
            id: "call-1".to_string(),
            arguments_delta: "{\"path\":".to_string(),
        };
        let (kind, payload) = protocol_event_payload(&event);
        assert_eq!(kind, "tool_call_arguments_delta");
        assert_eq!(payload["call_id"], "call-1");
        assert_eq!(payload["delta"], "{\"path\":");
        assert!(payload.get("pretty").is_none());
    }
}
