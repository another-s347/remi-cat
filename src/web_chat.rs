use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use bot_core::{
    todo::TodoItem, CatEvent, ContextMetrics, ModelInputSnapshot, SteerSubmitResult,
    ThreadHistoryMessage, TokenUsage, ToolApprovalDecision, ToolApprovalRequest,
    UserQuestionRequest, UserQuestionResponse,
};
use futures::StreamExt;
use remi_agentloop::prelude::CancellationToken;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::{
    model_input_store::upsert_model_input_snapshot_json, ChatChannel, ChatRequest, CoreChatEvent,
    Runtime, SESSION_AGENT_ID_METADATA_KEY, SESSION_MODEL_PROFILE_METADATA_KEY,
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
    pub text: String,
    pub started_at: String,
}

pub(crate) enum WebChatCommand {
    Run {
        session_id: String,
        run_id: String,
        text: String,
        response: oneshot::Sender<anyhow::Result<WebRun>>,
    },
    Steer {
        session_id: String,
        run_id: String,
        text: String,
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
    text: String,
    started_at: String,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
    direct: mpsc::Sender<ChatEventV1>,
    log: Rc<RefCell<Vec<ChatEventV1>>>,
    broadcast: broadcast::Sender<ChatEventV1>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatEventV1 {
    pub version: u8,
    pub event: &'static str,
    pub run_id: String,
    pub session_id: String,
    pub sequence: u64,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ChatEventV1 {
    fn new(
        event: &'static str,
        run_id: &str,
        session_id: &str,
        sequence: u64,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            version: 1,
            event,
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
        text: String,
    ) -> anyhow::Result<WebRun> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::Run {
                session_id,
                run_id,
                text,
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
        text: String,
    ) -> anyhow::Result<()> {
        let (response, rx) = oneshot::channel();
        self.tx
            .send(WebChatCommand::Steer {
                session_id,
                run_id,
                text,
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

    while let Some(command) = rx.recv().await {
        match command {
            WebChatCommand::Run {
                session_id,
                run_id,
                text,
                response,
            } => {
                if let Some(existing_run_id) =
                    active_run_id_for_session(&active.borrow(), &session_id)
                {
                    if existing_run_id == run_id {
                        let run = attach_active_run(&active.borrow(), &run_id);
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
                let text_for_task = text.clone();
                let log_for_task = Rc::clone(&log);
                let broadcast_for_task = broadcast_tx.clone();
                let cancel_for_task = cancel.clone();
                let handle = tokio::task::spawn_local(async move {
                    let sink = WebRunEventSink {
                        direct: events_tx,
                        log: log_for_task,
                        broadcast: broadcast_for_task,
                    };
                    run_turn(
                        runtime,
                        &session_id_for_task,
                        &run_id_for_task,
                        text_for_task,
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
                        text: text.clone(),
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
                text,
                response,
            } => {
                let run_matches_session = active
                    .borrow()
                    .get(&run_id)
                    .is_some_and(|run| run.session_id == session_id);
                if !run_matches_session {
                    let _ = response.send(Err(anyhow::anyhow!(
                        "no matching active run for this session"
                    )));
                    continue;
                }
                let request = ChatRequest::text(session_id.clone(), ChatChannel::Web, text.clone())
                    .with_sender(WEB_USER_ID, Some(WEB_USER_ID.to_string()))
                    .with_message(format!("web-steer-{}", uuid::Uuid::new_v4()), "p2p")
                    .with_platform(Some(WEB_CHANNEL.to_string()));
                match runtime.submit_steer(request) {
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
                            run.log.borrow_mut().push(web_event.clone());
                            let _ = run.broadcast.send(web_event.clone());
                            let _ = run.direct.try_send(web_event);
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
                    let _ = runtime.bot.cancel_background_tasks(&session_id).await;
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
                        text: run.text.clone(),
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
                        text: run.text.clone(),
                        started_at: run.started_at.clone(),
                    })
                    .collect();
                let _ = response.send(active_runs);
            }
            WebChatCommand::History {
                session_id,
                response,
            } => {
                let mut history = runtime.bot.thread_history(&session_id).await;
                if let Some(instance) = runtime.bot.workflow_status(&session_id).await {
                    append_supervisor_history(&mut history, &session_id, &instance);
                }
                let _ = response.send(history);
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
                let result = runtime
                    .bot
                    .delete_thread_data(&session_id, Some(WEB_USER_ID))
                    .await
                    .map_err(anyhow::Error::from);
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

fn attach_active_run(active: &HashMap<String, ActiveRun>, run_id: &str) -> Option<WebRun> {
    let run = active.get(run_id)?;
    let (tx, rx) = mpsc::channel(128);
    let mut broadcast_rx = run.broadcast.subscribe();
    let replay = run.log.borrow().clone();
    tokio::task::spawn_local(async move {
        for event in replay {
            if tx.send(event).await.is_err() {
                return;
            }
        }
        loop {
            match broadcast_rx.recv().await {
                Ok(event) => {
                    if tx.send(event).await.is_err() {
                        return;
                    }
                }
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
}

async fn send_event(sink: &WebRunEventSink, event: ChatEventV1) {
    sink.log.borrow_mut().push(event.clone());
    let _ = sink.broadcast.send(event.clone());
    let _ = sink.direct.send(event).await;
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
    Done,
    Skip,
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
    streaming_tools: HashMap<String, StreamingToolCall>,
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
            streaming_tools: HashMap::new(),
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
            CoreChatEvent::Prefix(text) | CoreChatEvent::Reply(text) => {
                self.mark_first_response();
                WebEventMap::Emit("text_delta", serde_json::json!({"text": text}))
            }
            CoreChatEvent::Done => WebEventMap::Done,
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
                self.streaming_tools.insert(
                    id.clone(),
                    StreamingToolCall {
                        name: name.clone(),
                        arguments: String::new(),
                        last_emit: Instant::now(),
                    },
                );
                let args = serde_json::Value::Object(serde_json::Map::new());
                let pretty = bot_core::PrettyToolCall::started(&id, &name, &args);
                WebEventMap::Emit(
                    "tool_started",
                    serde_json::json!({
                        "call_id": id,
                        "tool_name": name,
                        "args": args,
                        "pretty": pretty,
                        "partial": true,
                    }),
                )
            }
            CatEvent::ToolCallArgumentsDelta { id, delta } => {
                if let Some(call) = self.streaming_tools.get_mut(&id) {
                    call.arguments.push_str(&delta);
                    if call.last_emit.elapsed() < Duration::from_millis(500) {
                        WebEventMap::Skip
                    } else {
                        call.last_emit = Instant::now();
                        streaming_tool_progress(&id, call)
                            .map(|pretty| {
                                WebEventMap::Emit(
                                    "tool_started",
                                    serde_json::json!({
                                        "call_id": id,
                                        "tool_name": call.name.clone(),
                                        "args": {},
                                        "pretty": pretty,
                                        "partial": true,
                                    }),
                                )
                            })
                            .unwrap_or(WebEventMap::Skip)
                    }
                } else {
                    WebEventMap::Skip
                }
            }
            CatEvent::ToolCall { id, name, args } => {
                self.mark_first_response();
                self.streaming_tools.remove(&id);
                let pretty = bot_core::PrettyToolCall::started(&id, &name, &args);
                WebEventMap::Emit(
                    "tool_started",
                    serde_json::json!({
                        "call_id": id,
                        "tool_name": name,
                        "args": args,
                        "pretty": pretty,
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
            } => {
                self.streaming_tools.remove(&id);
                let pretty = bot_core::PrettyToolCall::completed(
                    &id, &name, &args, &result, success, elapsed_ms,
                );
                WebEventMap::Emit(
                    "tool_completed",
                    serde_json::json!({
                        "call_id": id,
                        "tool_name": name,
                        "args": args,
                        "result": result,
                        "success": success,
                        "elapsed_ms": elapsed_ms,
                        "pretty": pretty,
                    }),
                )
            }
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
            CatEvent::SubSession(event) => WebEventMap::Emit("sub_session", {
                let mut value = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert(
                        "thread_id".to_string(),
                        serde_json::Value::String(event.sub_thread_id.0.clone()),
                    );
                    map.insert(
                        "run_id".to_string(),
                        serde_json::Value::String(event.sub_run_id.0.clone()),
                    );
                }
                value
            }),
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
            CatEvent::Error(error) => {
                WebEventMap::Emit("error", serde_json::json!({"message": error.to_string()}))
            }
            CatEvent::Done => WebEventMap::Done,
            _ => WebEventMap::Skip,
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
    text: String,
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
                    ChatEventV1::new("run_finished", run_id, session_id, sequence, None),
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
                    ChatEventV1::new("run_finished", run_id, session_id, sequence, None),
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
    let context_tokens = runtime
        .bot
        .model_context_tokens_for_agent(model_profile_id.as_deref(), agent_id.as_deref());
    let request = ChatRequest::text(session_id.to_string(), ChatChannel::Web, text)
        .with_sender(WEB_USER_ID, Some(WEB_USER_ID.to_string()))
        .with_message(run_id.to_string(), "p2p")
        .with_platform(Some(WEB_CHANNEL.to_string()))
        .with_cancel(cancel);
    let mut stream = std::pin::pin!(Rc::clone(&runtime).chat(request));
    let timeout = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(timeout);
    let mut event_mapper = WebCoreEventMapper::new(context_tokens, run_started_at);

    loop {
        let event = tokio::select! {
            event = stream.next() => event,
            _ = &mut timeout => {
                send_event(&sink, ChatEventV1::new(
                    "error", run_id, session_id, sequence,
                    Some(serde_json::json!({"message": "reply timed out"})),
                )).await;
                sequence += 1;
                None
            }
        };
        let Some(event) = event else { break };
        match event_mapper.map(&runtime, session_id, event) {
            WebEventMap::Emit(kind, data) => {
                send_event(
                    &sink,
                    ChatEventV1::new(kind, run_id, session_id, sequence, Some(data)),
                )
                .await;
                sequence += 1;
            }
            WebEventMap::Done => break,
            WebEventMap::Skip => {}
        }
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
        ChatEventV1::new("run_finished", run_id, session_id, sequence, None),
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

struct StreamingToolCall {
    name: String,
    arguments: String,
    last_emit: Instant,
}

fn streaming_tool_progress(id: &str, call: &StreamingToolCall) -> Option<bot_core::PrettyToolCall> {
    match call.name.as_str() {
        "fs_write" | "fs_create" => {
            let path = partial_json_string(&call.arguments, "path", 160)
                .map(|value| value.value)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "文件".to_string());
            let content_bytes = partial_json_string(&call.arguments, "content", 0)
                .map(|value| value.unescaped_bytes)
                .unwrap_or(0);
            let verb = if call.name == "fs_create" {
                "创建"
            } else {
                "写入"
            };
            Some(bot_core::PrettyToolCall {
                id: id.to_string(),
                tool_name: call.name.clone(),
                title: format!("{verb} {path}"),
                summary: if content_bytes == 0 {
                    "正在生成写入内容".to_string()
                } else {
                    format!("已生成 {} 内容", format_bytes(content_bytes))
                },
                status: bot_core::PrettyToolStatus::Running,
                elapsed_ms: None,
                request: serde_json::Value::Object(serde_json::Map::new()),
                response: None,
            })
        }
        _ => None,
    }
}

struct PartialJsonString {
    value: String,
    unescaped_bytes: usize,
}

fn partial_json_string(input: &str, key: &str, value_limit: usize) -> Option<PartialJsonString> {
    let needle = format!("\"{key}\"");
    let start = input.find(&needle)? + needle.len();
    let bytes = input.as_bytes();
    let mut index = start;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if bytes.get(index) != Some(&b':') {
        return None;
    }
    index += 1;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    index += 1;

    let mut value = String::new();
    let mut unescaped_bytes = 0usize;
    let mut escaped = false;
    let mut unicode_remaining = 0usize;
    while index < input.len() {
        let ch = input[index..].chars().next()?;
        index += ch.len_utf8();
        if unicode_remaining > 0 {
            unicode_remaining -= 1;
            if unicode_remaining == 0 {
                unescaped_bytes += 1;
                if value.len() < value_limit {
                    value.push('?');
                }
            }
            continue;
        }
        if escaped {
            escaped = false;
            match ch {
                '"' | '\\' | '/' => {
                    unescaped_bytes += ch.len_utf8();
                    if value.len() < value_limit {
                        value.push(ch);
                    }
                }
                'b' | 'f' | 'n' | 'r' | 't' => {
                    unescaped_bytes += 1;
                    if value.len() < value_limit {
                        value.push(match ch {
                            'n' => '\n',
                            'r' => '\r',
                            't' => '\t',
                            _ => ' ',
                        });
                    }
                }
                'u' => {
                    unicode_remaining = 4;
                }
                _ => {
                    unescaped_bytes += ch.len_utf8();
                    if value.len() < value_limit {
                        value.push(ch);
                    }
                }
            }
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => break,
            _ => {
                unescaped_bytes += ch.len_utf8();
                if value.len() < value_limit {
                    value.push(ch);
                }
            }
        }
    }
    Some(PartialJsonString {
        value,
        unescaped_bytes,
    })
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} 字节")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        stats_payload, streaming_tool_progress, StreamingToolCall, WebCoreEventMapper, WebEventMap,
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
    fn streaming_tool_progress_extracts_partial_write_summary() {
        let call = StreamingToolCall {
            name: "fs_write".to_string(),
            arguments: r#"{"path":"src/lib.rs","content":"hello world"}"#.to_string(),
            last_emit: Instant::now(),
        };

        let pretty = streaming_tool_progress("call-1", &call).expect("progress");

        assert_eq!(pretty.id, "call-1");
        assert_eq!(pretty.tool_name, "fs_write");
        assert_eq!(pretty.title, "写入 src/lib.rs");
        assert_eq!(pretty.summary, "已生成 11 字节 内容");
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
}
