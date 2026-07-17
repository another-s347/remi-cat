use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex as StdMutex};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, CreateElicitationRequest, DeleteSessionRequest, DeleteSessionResponse,
    ElicitationAcceptAction, ElicitationAction, ElicitationContentValue, ElicitationFormMode,
    ElicitationSchema, ElicitationSessionScope, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, MessageId, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, ResumeSessionRequest, ResumeSessionResponse,
    SelectedPermissionOutcome, SessionAdditionalDirectoriesCapabilities, SessionCapabilities,
    SessionCloseCapabilities, SessionDeleteCapabilities, SessionInfo, SessionListCapabilities,
    SessionNotification, SessionResumeCapabilities, SessionUpdate, StopReason, TextContent,
    ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind, UsageUpdate,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Stdio};
use bot_core::{
    CatEvent, ToolApprovalDecision, ToolApprovalRequest, UserQuestionRequest, UserQuestionResponse,
    UserQuestionStatus,
};
use futures::StreamExt;
use remi_agentloop::prelude::{CancellationToken, ProtocolEvent, SubSessionEvent};
use tokio::sync::Mutex;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::core::{ChatChannel, ChatRequest, CoreChatEvent, Runtime};
use crate::secret_store::{redaction_entries, SecretStore};
use crate::session::SessionRuntime;
use bot_core::im_tools::ImFileBridge;
use bot_core::{AcpClientToolProvider, AcpClientToolSupport, CatBotBuilder};
use user_store::UserStore;

type CancelRegistry = Arc<StdMutex<HashMap<String, CancellationToken>>>;
type AcpSessions = Arc<Mutex<SessionRuntime>>;
type ElicitationSupport = Arc<StdMutex<bool>>;
type AcpClientToolSupportState = Arc<StdMutex<AcpClientToolSupport>>;

const ACP_CHANNEL: &str = "acp";
const ACP_CWD_METADATA_KEY: &str = "acp_cwd";
const ACP_ADDITIONAL_DIRECTORIES_METADATA_KEY: &str = "acp_additional_directories";
const PERMISSION_DENY: &str = "deny";
const PERMISSION_ALLOW_ONCE: &str = "allow_once";
const PERMISSION_ALLOW_SAME_COMMAND_SESSION: &str = "allow_same_command_session";
const PERMISSION_ALLOW_RISK_LEVEL_SESSION: &str = "allow_risk_level_session";

pub(crate) struct AcpRuntimeFactory {
    data_dir: PathBuf,
    secret_store: Arc<Mutex<SecretStore>>,
    sessions: AcpSessions,
    root_agent_id: String,
    runtime: RefCell<Option<Rc<Runtime>>>,
    workspace_dir: RefCell<Option<PathBuf>>,
    acp_client_tool_provider: AcpClientToolProvider,
    acp_client_tool_support: AcpClientToolSupportState,
}

impl AcpRuntimeFactory {
    pub(crate) fn new(
        data_dir: PathBuf,
        secret_store: Arc<Mutex<SecretStore>>,
        sessions: AcpSessions,
        root_agent_id: String,
    ) -> Self {
        Self {
            data_dir,
            secret_store,
            sessions,
            root_agent_id,
            runtime: RefCell::new(None),
            workspace_dir: RefCell::new(None),
            acp_client_tool_provider: AcpClientToolProvider::new(),
            acp_client_tool_support: Arc::new(StdMutex::new(AcpClientToolSupport::default())),
        }
    }

    async fn runtime_for_session(&self, session_id: &str) -> anyhow::Result<Rc<Runtime>> {
        if let Some(runtime) = self.runtime.borrow().as_ref() {
            self.warn_if_workspace_differs(session_id).await;
            return Ok(Rc::clone(runtime));
        }
        let workspace_dir = self
            .session_cwd(session_id)
            .await
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| self.data_dir.clone()));
        tracing::info!(
            session_id,
            data_dir = %self.data_dir.display(),
            workspace_dir = %workspace_dir.display(),
            env_data_dir = std::env::var("REMI_DATA_DIR").unwrap_or_default(),
            env_model_profile = std::env::var("REMI_MODEL_PROFILE").unwrap_or_default(),
            env_agent_id = std::env::var("REMI_AGENT_ID").unwrap_or_default(),
            "ACP runtime build starting"
        );
        self.configure_workspace_env(&workspace_dir);
        let bridge: Arc<dyn ImFileBridge> =
            Arc::new(crate::channel::feishu::LocalImFileBridge::new(None));
        let bot = Rc::new(
            CatBotBuilder::from_env()?
                .im_bridge(Arc::clone(&bridge))
                .acp_client_tools(
                    self.acp_client_tool_provider.clone(),
                    self.acp_client_tool_support
                        .lock()
                        .map(|support| *support)
                        .unwrap_or_default(),
                )
                .build()?,
        );
        let default_model = bot.default_model_profile();
        tracing::info!(
            session_id,
            data_dir = %self.data_dir.display(),
            workspace_dir = %workspace_dir.display(),
            default_model_profile = %default_model.id,
            default_model = %default_model.model,
            default_model_base_url = default_model.base_url.as_deref().unwrap_or(""),
            "ACP runtime build completed"
        );
        bot.update_secret_redactor(&redaction_entries(
            &self.secret_store.lock().await.entries()?,
        ));
        let runtime = Rc::new(Runtime {
            bot,
            secret_store: Arc::clone(&self.secret_store),
            user_store: Arc::new(UserStore::load(self.data_dir.join("users.json"))?),
            sessions: Arc::clone(&self.sessions),
            im_bridge: Arc::clone(&bridge),
            root_agent_id: self.root_agent_id.clone(),
            data_dir: self.data_dir.clone(),
        });
        self.workspace_dir.replace(Some(workspace_dir));
        self.runtime.replace(Some(Rc::clone(&runtime)));
        Ok(runtime)
    }

    async fn session_cwd(&self, session_id: &str) -> Option<PathBuf> {
        self.sessions
            .lock()
            .await
            .metadata_string(session_id, ACP_CWD_METADATA_KEY)
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
    }

    async fn warn_if_workspace_differs(&self, session_id: &str) {
        let Some(existing) = self.workspace_dir.borrow().clone() else {
            return;
        };
        let Some(requested) = self.session_cwd(session_id).await else {
            return;
        };
        if requested != existing {
            tracing::warn!(
                session_id,
                existing_workspace = %existing.display(),
                requested_workspace = %requested.display(),
                "ACP session cwd differs after runtime initialization; keeping existing workspace"
            );
        }
    }

    fn configure_workspace_env(&self, workspace_dir: &Path) {
        unsafe {
            std::env::set_var("REMI_SANDBOX_HOST_DIR", workspace_dir);
        }
        if let Err(error) = std::env::set_current_dir(workspace_dir) {
            tracing::warn!(
                workspace_dir = %workspace_dir.display(),
                error = %error,
                "failed to set ACP process cwd"
            );
        }
        tracing::info!(
            workspace_dir = %workspace_dir.display(),
            "ACP workspace cwd applied"
        );
    }
}

pub(crate) async fn run_stdio_agent(factory: Rc<AcpRuntimeFactory>) -> anyhow::Result<()> {
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<PromptJob>();
    let cancellations: CancelRegistry = Arc::new(StdMutex::new(HashMap::new()));
    let sessions = Arc::clone(&factory.sessions);
    let root_agent_id = factory.root_agent_id.clone();
    let elicitation_support: ElicitationSupport = Arc::new(StdMutex::new(false));
    let acp_client_tool_support = Arc::clone(&factory.acp_client_tool_support);

    let local = tokio::task::LocalSet::new();
    local.spawn_local({
        let factory = Rc::clone(&factory);
        async move {
            while let Some(job) = prompt_rx.recv().await {
                let acp_session_id = job.request.session_id.to_string();
                let result = match factory.runtime_for_session(&acp_session_id).await {
                    Ok(runtime) => {
                        run_prompt_turn(
                            runtime,
                            job.request,
                            job.connection,
                            job.cancel,
                            job.elicitation_supported,
                            factory.acp_client_tool_provider.clone(),
                        )
                        .await
                    }
                    Err(error) => Err(agent_client_protocol::util::internal_error(
                        error.to_string(),
                    )),
                };
                if let Ok(mut map) = job.cancellations.lock() {
                    map.remove(&acp_session_id);
                }
                let _ = job.response.send(result);
            }
        }
    });

    let agent = Agent
        .builder()
        .name("remi-cat")
        .on_receive_request(
            {
                let elicitation_support = Arc::clone(&elicitation_support);
                let acp_client_tool_support = Arc::clone(&acp_client_tool_support);
                async move |initialize: InitializeRequest, responder, _cx| {
                    if let Ok(mut supported) = elicitation_support.lock() {
                        *supported = initialize.client_capabilities.elicitation.is_some();
                    }
                    let support = AcpClientToolSupport {
                        fs_read: initialize.client_capabilities.fs.read_text_file,
                        fs_write: initialize.client_capabilities.fs.write_text_file,
                        terminal: initialize.client_capabilities.terminal,
                    };
                    if let Ok(mut current) = acp_client_tool_support.lock() {
                        *current = support;
                    }
                    tracing::info!(
                        fs_read = support.fs_read,
                        fs_write = support.fs_write,
                        terminal = support.terminal,
                        elicitation = initialize.client_capabilities.elicitation.is_some(),
                        "ACP client capabilities received"
                    );
                    let capabilities = AgentCapabilities::new()
                        .load_session(true)
                        .session_capabilities(
                            SessionCapabilities::new()
                                .resume(Some(SessionResumeCapabilities::new()))
                                .list(Some(SessionListCapabilities::new()))
                                .delete(Some(SessionDeleteCapabilities::new()))
                                .close(Some(SessionCloseCapabilities::new()))
                                .additional_directories(Some(
                                    SessionAdditionalDirectoriesCapabilities::new(),
                                )),
                        );
                    responder.respond(
                        InitializeResponse::new(initialize.protocol_version)
                            .agent_capabilities(capabilities)
                            .agent_info(Some(
                                Implementation::new("remi-cat", env!("CARGO_PKG_VERSION"))
                                    .title(Some("remi-cat".to_string())),
                            )),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let root_agent_id = root_agent_id.clone();
                async move |request: NewSessionRequest, responder, _cx| {
                    tracing::info!(
                        cwd = %request.cwd.display(),
                        additional_directories = request.additional_directories.len(),
                        mcp_servers = request.mcp_servers.len(),
                        root_agent_id = %root_agent_id,
                        "ACP new_session received"
                    );
                    let response = create_acp_session(&sessions, &root_agent_id, &request)
                        .await
                        .map(NewSessionResponse::new)
                        .map_err(|err| {
                            agent_client_protocol::util::internal_error(err.to_string())
                        })?;
                    responder.respond(response)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let root_agent_id = root_agent_id.clone();
                async move |request: LoadSessionRequest, responder, _cx| {
                    tracing::info!(
                        session_id = %request.session_id,
                        cwd = %request.cwd.display(),
                        additional_directories = request.additional_directories.len(),
                        mcp_servers = request.mcp_servers.len(),
                        root_agent_id = %root_agent_id,
                        "ACP load_session received"
                    );
                    ensure_acp_session(
                        &sessions,
                        request.session_id.to_string().as_str(),
                        &root_agent_id,
                        Some(&request.cwd),
                        Some(&request.additional_directories),
                    )
                    .await
                    .map_err(|err| agent_client_protocol::util::internal_error(err.to_string()))?;
                    responder.respond(LoadSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let root_agent_id = root_agent_id.clone();
                async move |request: ResumeSessionRequest, responder, _cx| {
                    tracing::info!(
                        session_id = %request.session_id,
                        cwd = %request.cwd.display(),
                        additional_directories = request.additional_directories.len(),
                        mcp_servers = request.mcp_servers.len(),
                        root_agent_id = %root_agent_id,
                        "ACP session_resume received"
                    );
                    ensure_acp_session(
                        &sessions,
                        request.session_id.to_string().as_str(),
                        &root_agent_id,
                        Some(&request.cwd),
                        Some(&request.additional_directories),
                    )
                    .await
                    .map_err(|err| agent_client_protocol::util::internal_error(err.to_string()))?;
                    responder.respond(ResumeSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                async move |request: ListSessionsRequest, responder, _cx| {
                    let response = list_acp_sessions(&sessions, request.cwd.as_deref())
                        .await
                        .map(ListSessionsResponse::new)
                        .map_err(|err| {
                            agent_client_protocol::util::internal_error(err.to_string())
                        })?;
                    responder.respond(response)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = Arc::clone(&sessions);
                let cancellations = Arc::clone(&cancellations);
                async move |request: DeleteSessionRequest, responder, _cx| {
                    cancel_acp_session(&cancellations, request.session_id.to_string().as_str());
                    delete_acp_session(&sessions, request.session_id.to_string().as_str())
                        .await
                        .map_err(|err| {
                            agent_client_protocol::util::internal_error(err.to_string())
                        })?;
                    responder.respond(DeleteSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let cancellations = Arc::clone(&cancellations);
                async move |request: CloseSessionRequest, responder, _cx| {
                    cancel_acp_session(&cancellations, request.session_id.to_string().as_str());
                    responder.respond(CloseSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let prompt_tx = prompt_tx.clone();
                let cancellations = Arc::clone(&cancellations);
                let elicitation_support = Arc::clone(&elicitation_support);
                async move |request: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let (response_tx, response_rx) = oneshot::channel();
                    let cancel = CancellationToken::new();
                    let session_id = request.session_id.to_string();
                    tracing::info!(
                        session_id = %session_id,
                        content_chunks = request.prompt.len(),
                        "ACP prompt received"
                    );
                    cancellations
                        .lock()
                        .map_err(|_| {
                            agent_client_protocol::util::internal_error(
                                "ACP cancellation registry is poisoned",
                            )
                        })?
                        .insert(session_id, cancel.clone());
                    let elicitation_supported = elicitation_support
                        .lock()
                        .map(|supported| *supported)
                        .unwrap_or(false);
                    prompt_tx
                        .send(PromptJob {
                            request,
                            connection: cx,
                            cancel,
                            cancellations: Arc::clone(&cancellations),
                            elicitation_supported,
                            response: response_tx,
                        })
                        .map_err(|_| {
                            agent_client_protocol::util::internal_error(
                                "ACP prompt worker is no longer running",
                            )
                        })?;
                    tokio::task::spawn_local(async move {
                        let response = response_rx.await.map_err(|_| {
                            agent_client_protocol::util::internal_error(
                                "ACP prompt worker dropped the response",
                            )
                        });
                        let result = match response {
                            Ok(Ok(response)) => responder.respond(response),
                            Ok(Err(error)) => responder.respond_with_error(error),
                            Err(error) => responder.respond_with_error(error),
                        };
                        if let Err(error) = result {
                            tracing::warn!(error = %error, "failed to send ACP prompt response");
                        }
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let cancellations = Arc::clone(&cancellations);
                async move |cancel: CancelNotification, _cx| {
                    if let Ok(map) = cancellations.lock() {
                        if let Some(notify) = map.get(&cancel.session_id.to_string()) {
                            notify.cancel();
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled ACP message"),
                    cx,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new());

    local
        .run_until(agent)
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

struct PromptJob {
    request: PromptRequest,
    connection: ConnectionTo<Client>,
    cancel: CancellationToken,
    cancellations: CancelRegistry,
    elicitation_supported: bool,
    response: oneshot::Sender<agent_client_protocol::Result<PromptResponse>>,
}

async fn run_prompt_turn(
    runtime: Rc<Runtime>,
    request: PromptRequest,
    cx: ConnectionTo<Client>,
    cancel: CancellationToken,
    elicitation_supported: bool,
    acp_client_tool_provider: AcpClientToolProvider,
) -> agent_client_protocol::Result<PromptResponse> {
    let session_id = request.session_id.clone();
    let prompt_text = prompt_to_text(request.prompt)?;
    let thread_id = ensure_acp_session(
        &runtime.sessions,
        session_id.to_string().as_str(),
        &runtime.root_agent_id,
        None,
        None,
    )
    .await
    .map_err(|err| agent_client_protocol::util::internal_error(err.to_string()))?;
    set_acp_title_if_empty(&runtime.sessions, &thread_id, &prompt_text)
        .await
        .map_err(|err| agent_client_protocol::util::internal_error(err.to_string()))?;
    let cwd = runtime
        .sessions
        .lock()
        .await
        .metadata_string(&thread_id, ACP_CWD_METADATA_KEY)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| runtime.data_dir.clone()));
    let _acp_tool_turn = AcpClientToolTurnGuard::new(
        acp_client_tool_provider,
        cx.clone(),
        session_id.clone(),
        cwd,
    );
    let session_model_profile = runtime
        .sessions
        .lock()
        .await
        .metadata_string(&thread_id, crate::app::SESSION_MODEL_PROFILE_METADATA_KEY);
    let default_model = runtime.bot.default_model_profile();
    tracing::info!(
        session_id = %session_id,
        thread_id = %thread_id,
        data_dir = %runtime.data_dir.display(),
        root_agent_id = %runtime.root_agent_id,
        session_model_profile = session_model_profile.as_deref().unwrap_or(""),
        default_model_profile = %default_model.id,
        default_model = %default_model.model,
        default_model_base_url = default_model.base_url.as_deref().unwrap_or(""),
        prompt_chars = prompt_text.chars().count(),
        "ACP prompt turn starting"
    );

    let message_id = MessageId::new(Uuid::new_v4().to_string());
    let mut forwarder = AcpEventForwarder {
        runtime: Rc::clone(&runtime),
        cx,
        session_id: session_id.clone(),
        message_id: message_id.clone(),
        announced_tools: HashSet::new(),
        announced_sub_sessions: HashSet::new(),
        sub_session_summaries: HashMap::new(),
        elicitation_supported,
    };
    tokio::task::yield_now().await;

    let chat_request = ChatRequest::text(thread_id, ChatChannel::Acp, prompt_text)
        .with_sender("zed", Some("Zed".to_string()))
        .with_message(message_id.to_string(), ACP_CHANNEL)
        .with_platform(Some(ACP_CHANNEL.to_string()))
        .with_cancel(cancel.clone());
    let mut stream = std::pin::pin!(runtime.chat(chat_request));
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            event = stream.next() => event,
        };
        let Some(event) = event else {
            return Err(agent_client_protocol::util::internal_error(
                "agent stream ended without done",
            ));
        };
        match event {
            CoreChatEvent::Done => return Ok(PromptResponse::new(StopReason::EndTurn)),
            other => match forwarder.forward(other).await? {
                ForwardStatus::Continue => {}
                ForwardStatus::Cancelled => {
                    return Ok(PromptResponse::new(StopReason::Cancelled));
                }
                ForwardStatus::Error(error) => {
                    return Err(agent_client_protocol::util::internal_error(error));
                }
            },
        }
    }
}

struct AcpClientToolTurnGuard {
    provider: AcpClientToolProvider,
}

impl AcpClientToolTurnGuard {
    fn new(
        provider: AcpClientToolProvider,
        connection: ConnectionTo<Client>,
        session_id: agent_client_protocol::schema::v1::SessionId,
        cwd: PathBuf,
    ) -> Self {
        provider.set_turn(connection, session_id, cwd);
        Self { provider }
    }
}

impl Drop for AcpClientToolTurnGuard {
    fn drop(&mut self) {
        self.provider.clear_turn();
    }
}

struct AcpEventForwarder {
    runtime: Rc<Runtime>,
    cx: ConnectionTo<Client>,
    session_id: agent_client_protocol::schema::v1::SessionId,
    message_id: MessageId,
    announced_tools: HashSet<String>,
    announced_sub_sessions: HashSet<String>,
    sub_session_summaries: HashMap<String, String>,
    elicitation_supported: bool,
}

enum ForwardStatus {
    Continue,
    Cancelled,
    Error(String),
}

impl AcpEventForwarder {
    async fn forward(
        &mut self,
        event: CoreChatEvent,
    ) -> agent_client_protocol::Result<ForwardStatus> {
        match event {
            CoreChatEvent::SupervisorStarted => {}
            CoreChatEvent::Prefix(text) | CoreChatEvent::Reply(text) => {
                self.send_message_chunk(text)?;
            }
            CoreChatEvent::Done => {}
            CoreChatEvent::Bot(event) => match event {
                CatEvent::Text(delta) => self.send_message_chunk(delta)?,
                CatEvent::Thinking(delta) => self.send_thought_chunk(delta)?,
                CatEvent::ToolCallStart { id, name } => {
                    self.announced_tools.insert(id.clone());
                    send_tool_call(&self.cx, self.session_id.clone(), id, name, None)?;
                }
                CatEvent::ToolCallArgumentsDelta { .. } => {}
                CatEvent::ToolCall { id, name, args } => {
                    if self.announced_tools.insert(id.clone()) {
                        send_tool_call(&self.cx, self.session_id.clone(), id, name, Some(args))?;
                    } else {
                        send_tool_input_update(&self.cx, self.session_id.clone(), id, name, args)?;
                    }
                }
                CatEvent::ToolCallResult {
                    id,
                    name,
                    args: _,
                    result,
                    success,
                    elapsed_ms: _,
                } => {
                    send_tool_result(&self.cx, self.session_id.clone(), id, name, result, success)?;
                }
                CatEvent::ToolTaskCompleted(task) => {
                    self.send_thought_chunk(format!(
                        "Background task {} ({}) completed with status {} in {}ms.",
                        task.task_id,
                        task.tool_name,
                        task.status,
                        task.elapsed_ms.unwrap_or(0)
                    ))?;
                }
                CatEvent::ToolApprovalRequested(request) => {
                    self.handle_tool_approval(request).await?;
                }
                CatEvent::ToolApprovalUpdated(request) => {
                    self.send_thought_chunk(format!(
                        "Tool approval updated: {} ({:?})",
                        request.tool_name, request.risk
                    ))?;
                }
                CatEvent::ToolApprovalResolved { request, decision } => {
                    self.send_thought_chunk(format!(
                        "Tool approval resolved: {} -> {:?}",
                        request.tool_name, decision
                    ))?;
                }
                CatEvent::UserQuestionRequested(request) => {
                    self.handle_user_question(request).await?;
                }
                CatEvent::UserQuestionUpdated(request) => {
                    self.send_thought_chunk(format!(
                        "User question updated: {}",
                        request.question
                    ))?;
                }
                CatEvent::UserQuestionResolved { request, response } => {
                    self.send_thought_chunk(format!(
                        "User question resolved: {} -> {:?}",
                        request.question, response.status
                    ))?;
                }
                CatEvent::Stats {
                    prompt_tokens,
                    completion_tokens,
                    max_prompt_tokens,
                    elapsed_ms: _,
                } => {
                    let used = u64::from(prompt_tokens) + u64::from(completion_tokens);
                    self.cx.send_notification(SessionNotification::new(
                        self.session_id.clone(),
                        SessionUpdate::UsageUpdate(UsageUpdate::new(
                            used,
                            u64::from(max_prompt_tokens),
                        )),
                    ))?;
                }
                CatEvent::Cancelled => return Ok(ForwardStatus::Cancelled),
                CatEvent::UserInterrupted { reason } => {
                    self.send_thought_chunk(reason)?;
                    return Ok(ForwardStatus::Cancelled);
                }
                CatEvent::Error(err) => return Ok(ForwardStatus::Error(err.to_string())),
                CatEvent::SubSession(event) => self.handle_sub_session_event(event)?,
                CatEvent::Supervisor(event) => self.send_thought_chunk(format!("{event:?}"))?,
                CatEvent::SupervisorProgress(event) => {
                    self.send_thought_chunk(format!("{event:?}"))?
                }
                CatEvent::ContextCompaction(event) => {
                    self.send_thought_chunk(format!("{event:?}"))?
                }
                CatEvent::SteerQueued(event) => self.send_thought_chunk(format!("{event:?}"))?,
                CatEvent::SteerInjected(event) => self.send_thought_chunk(format!("{event:?}"))?,
                CatEvent::StateUpdate(event) => self.send_thought_chunk(format!("{event:?}"))?,
                CatEvent::Done => {}
                CatEvent::History(..)
                | CatEvent::Skill(_)
                | CatEvent::Todo(_)
                | CatEvent::BackgroundTasksWaiting { .. }
                | CatEvent::ModelInputSnapshot(_) => {}
            },
        }
        Ok(ForwardStatus::Continue)
    }

    fn send_message_chunk(&self, text: String) -> agent_client_protocol::Result<()> {
        send_content_chunk(
            &self.cx,
            self.session_id.clone(),
            SessionUpdate::AgentMessageChunk,
            self.message_id.clone(),
            text,
        )
    }

    fn send_thought_chunk(&self, text: String) -> agent_client_protocol::Result<()> {
        send_content_chunk(
            &self.cx,
            self.session_id.clone(),
            SessionUpdate::AgentThoughtChunk,
            self.message_id.clone(),
            text,
        )
    }

    async fn handle_tool_approval(
        &mut self,
        request: ToolApprovalRequest,
    ) -> agent_client_protocol::Result<()> {
        self.send_thought_chunk(format!(
            "Tool approval requested: {} ({:?})",
            request.tool_name, request.risk
        ))?;
        let decision = request_permission(&self.cx, self.session_id.clone(), &request).await;
        let decided = self
            .runtime
            .bot
            .approval_manager()
            .decide(&request.id, decision)
            .await
            .is_some();
        tracing::info!(
            approval_id = %request.id,
            tool_name = %request.tool_name,
            session_id = %request.session_id,
            decision = ?decision,
            decided,
            source = "acp",
            "tool_approval.decision"
        );
        Ok(())
    }

    async fn handle_user_question(
        &mut self,
        request: UserQuestionRequest,
    ) -> agent_client_protocol::Result<()> {
        self.send_thought_chunk(format!("User input requested: {}", request.question))?;
        let response = if self.elicitation_supported {
            request_user_input(&self.cx, self.session_id.clone(), &request).await
        } else {
            self.send_thought_chunk(
                "ACP client does not advertise elicitation support; cancelling user question."
                    .to_string(),
            )?;
            cancelled_user_question_response(&request)
        };
        let answered = self
            .runtime
            .bot
            .answer_user_question(&request.id, response)
            .await
            .is_some();
        tracing::info!(
            question_id = %request.id,
            session_id = %request.session_id,
            answered,
            source = "acp",
            "user_question.answer"
        );
        Ok(())
    }

    fn handle_sub_session_event(
        &mut self,
        event: SubSessionEvent,
    ) -> agent_client_protocol::Result<()> {
        let id = sub_session_tool_id(&event);
        if self.announced_sub_sessions.insert(id.clone()) {
            let mut call = ToolCall::new(id.clone(), sub_session_tool_name(&event))
                .kind(ToolKind::Other)
                .status(ToolCallStatus::InProgress)
                .raw_input(serde_json::json!({
                    "agent": &event.agent_name,
                    "title": &event.title,
                    "thread_id": &event.sub_thread_id.0,
                    "run_id": &event.sub_run_id.0,
                    "depth": event.depth,
                    "parent_tool_call_id": &event.parent_tool_call_id,
                }));
            if matches!(event.event.as_ref(), ProtocolEvent::RunStart { .. }) {
                call = call.content(vec![ToolCallContent::from(ContentBlock::Text(
                    TextContent::new(sub_session_header(&event)),
                ))]);
            }
            self.cx.send_notification(SessionNotification::new(
                self.session_id.clone(),
                SessionUpdate::ToolCall(call),
            ))?;
        }

        let line = sub_session_event_line(&event);
        if let Some(line) = line.filter(|line| !line.trim().is_empty()) {
            let summary = self.sub_session_summaries.entry(id.clone()).or_default();
            if !summary.is_empty() {
                summary.push('\n');
            }
            summary.push_str(&line);
            if summary.len() > 12_000 {
                let keep_from = summary.len().saturating_sub(10_000);
                let trimmed = summary[keep_from..].trim_start().to_string();
                *summary = format!("...\n{trimmed}");
            }
        }

        let status = sub_session_tool_status(event.event.as_ref());
        let content = self
            .sub_session_summaries
            .get(&id)
            .cloned()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| sub_session_header(&event));
        let fields = ToolCallUpdateFields::new()
            .title(sub_session_title(&event))
            .status(status)
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new(content),
            ))]);
        self.cx.send_notification(SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields)),
        ))
    }
}

fn sub_session_tool_id(event: &SubSessionEvent) -> String {
    format!("sub-session:{}", event.sub_thread_id.0)
}

fn sub_session_tool_name(event: &SubSessionEvent) -> String {
    if event.agent_name.trim().is_empty() {
        "subagent".to_string()
    } else {
        format!("subagent__{}", event.agent_name.trim().replace('-', "_"))
    }
}

fn sub_session_title(event: &SubSessionEvent) -> String {
    event
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{} {}", event.agent_name, event.sub_thread_id.0))
}

fn sub_session_header(event: &SubSessionEvent) -> String {
    format!(
        "Subagent `{}` started.\nthread: `{}`\nrun: `{}`",
        event.agent_name, event.sub_thread_id.0, event.sub_run_id.0
    )
}

fn sub_session_tool_status(event: &ProtocolEvent) -> ToolCallStatus {
    match event {
        ProtocolEvent::Done => ToolCallStatus::Completed,
        ProtocolEvent::Custom { event_type, .. }
            if matches!(
                event_type.as_str(),
                "sub_session_done" | "sub_session_handed_off"
            ) =>
        {
            ToolCallStatus::Completed
        }
        ProtocolEvent::Error { .. } | ProtocolEvent::Cancelled => ToolCallStatus::Failed,
        _ => ToolCallStatus::InProgress,
    }
}

fn sub_session_event_line(event: &SubSessionEvent) -> Option<String> {
    let line = match event.event.as_ref() {
        ProtocolEvent::RunStart { .. } => sub_session_header(event),
        ProtocolEvent::Delta { content, .. } => content.clone(),
        ProtocolEvent::ThinkingStart => "Thinking...".to_string(),
        ProtocolEvent::ThinkingEnd { content } => {
            if content.trim().is_empty() {
                "Thinking complete.".to_string()
            } else {
                format!("Thinking:\n{content}")
            }
        }
        ProtocolEvent::TurnStart { turn } => format!("Turn {turn}"),
        ProtocolEvent::ToolCallStart { id, name } => {
            format!("Tool `{name}` started ({id}).")
        }
        ProtocolEvent::ToolCallDelta {
            id,
            arguments_delta,
        } => {
            format!("Tool arguments `{id}`:\n{arguments_delta}")
        }
        ProtocolEvent::ToolDelta { id, name, delta } => {
            format!("Tool `{name}` output ({id}):\n{delta}")
        }
        ProtocolEvent::ToolResult { id, name, result } => {
            format!("Tool `{name}` result ({id}):\n{result}")
        }
        ProtocolEvent::Done => "Done.".to_string(),
        ProtocolEvent::Custom { event_type, extra }
            if matches!(
                event_type.as_str(),
                "sub_session_done" | "sub_session_handed_off"
            ) =>
        {
            let label = if event_type == "sub_session_handed_off" {
                "Handed off"
            } else {
                "Done"
            };
            extra
                .get("final_output")
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| format!("{label}:\n{value}"))
                .unwrap_or_else(|| format!("{label}."))
        }
        ProtocolEvent::Error { message, .. } => format!("Error: {message}"),
        ProtocolEvent::Cancelled => "Error: cancelled".to_string(),
        other => format!("{other:?}"),
    };
    (!line.trim().is_empty()).then_some(line)
}

async fn create_acp_session(
    sessions: &AcpSessions,
    root_agent_id: &str,
    request: &NewSessionRequest,
) -> anyhow::Result<String> {
    if !request.mcp_servers.is_empty() {
        tracing::info!(
            mcp_servers = request.mcp_servers.len(),
            "ACP new_session mcp_servers ignored; MCP over ACP is unsupported"
        );
    }
    let title = acp_session_title(Some(&request.cwd));
    let initial_channel_id = format!("zed:{}", Uuid::new_v4());
    let mut sessions = sessions.lock().await;
    let session =
        sessions.create_channel(ACP_CHANNEL, &initial_channel_id, root_agent_id, title)?;
    sessions.set_channel_binding(&session.id, ACP_CHANNEL, &session.id)?;
    set_acp_session_cwd(&mut sessions, &session.id, &request.cwd)?;
    set_acp_additional_directories(&mut sessions, &session.id, &request.additional_directories)?;
    Ok(session.id)
}

async fn ensure_acp_session(
    sessions: &AcpSessions,
    acp_session_id: &str,
    root_agent_id: &str,
    cwd: Option<&Path>,
    additional_directories: Option<&[PathBuf]>,
) -> anyhow::Result<String> {
    let acp_session_id = acp_session_id.trim();
    if acp_session_id.is_empty() {
        anyhow::bail!("ACP session id is empty");
    }
    let mut sessions = sessions.lock().await;
    if let Some(existing) = sessions.get(acp_session_id) {
        if existing.channel_binding.platform != ACP_CHANNEL {
            anyhow::bail!("session `{acp_session_id}` is not an ACP session");
        }
        sessions.set_channel_binding(acp_session_id, ACP_CHANNEL, acp_session_id)?;
        if let Some(cwd) = cwd {
            set_acp_session_cwd(&mut sessions, acp_session_id, cwd)?;
        }
        if let Some(additional_directories) = additional_directories {
            set_acp_additional_directories(&mut sessions, acp_session_id, additional_directories)?;
        }
        return Ok(acp_session_id.to_string());
    }
    let session = sessions.create_channel(
        ACP_CHANNEL,
        acp_session_id,
        root_agent_id,
        acp_session_title(cwd),
    )?;
    sessions.set_channel_binding(&session.id, ACP_CHANNEL, &session.id)?;
    if let Some(cwd) = cwd {
        set_acp_session_cwd(&mut sessions, &session.id, cwd)?;
    }
    if let Some(additional_directories) = additional_directories {
        set_acp_additional_directories(&mut sessions, &session.id, additional_directories)?;
    }
    Ok(session.id)
}

fn set_acp_session_cwd(
    sessions: &mut SessionRuntime,
    session_id: &str,
    cwd: &Path,
) -> anyhow::Result<()> {
    if cwd.as_os_str().is_empty() {
        return Ok(());
    }
    sessions.set_metadata_string(session_id, ACP_CWD_METADATA_KEY, &cwd.display().to_string())?;
    Ok(())
}

fn set_acp_additional_directories(
    sessions: &mut SessionRuntime,
    session_id: &str,
    directories: &[PathBuf],
) -> anyhow::Result<()> {
    let absolute = directories
        .iter()
        .filter_map(|path| {
            if path.is_absolute() {
                Some(serde_json::Value::String(path.display().to_string()))
            } else {
                tracing::warn!(
                    session_id,
                    path = %path.display(),
                    "ignoring non-absolute ACP additionalDirectory"
                );
                None
            }
        })
        .collect::<Vec<_>>();
    sessions.set_metadata_value(
        session_id,
        ACP_ADDITIONAL_DIRECTORIES_METADATA_KEY,
        serde_json::Value::Array(absolute),
    )?;
    Ok(())
}

async fn list_acp_sessions(
    sessions: &AcpSessions,
    cwd_filter: Option<&Path>,
) -> anyhow::Result<Vec<SessionInfo>> {
    let sessions = sessions.lock().await;
    let mut listed = sessions
        .list()
        .into_iter()
        .filter(|session| session.channel_binding.platform == ACP_CHANNEL)
        .filter_map(|session| {
            let cwd = sessions
                .metadata_string(&session.id, ACP_CWD_METADATA_KEY)
                .map(PathBuf::from)?;
            if cwd_filter.is_some_and(|filter| filter != cwd.as_path()) {
                return None;
            }
            Some(
                SessionInfo::new(session.id, cwd)
                    .additional_directories(acp_additional_directories(&session.metadata))
                    .title(session.title)
                    .updated_at(Some(session.updated_at)),
            )
        })
        .collect::<Vec<_>>();
    listed.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(listed)
}

fn acp_additional_directories(
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> Vec<PathBuf> {
    metadata
        .get(ACP_ADDITIONAL_DIRECTORIES_METADATA_KEY)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .collect()
}

async fn delete_acp_session(sessions: &AcpSessions, session_id: &str) -> anyhow::Result<()> {
    let mut sessions = sessions.lock().await;
    let Some(session) = sessions.get(session_id) else {
        return Ok(());
    };
    if session.channel_binding.platform != ACP_CHANNEL {
        anyhow::bail!("session `{session_id}` is not an ACP session");
    }
    sessions.delete(session_id)?;
    Ok(())
}

fn cancel_acp_session(cancellations: &CancelRegistry, session_id: &str) {
    if let Ok(mut map) = cancellations.lock() {
        if let Some(notify) = map.remove(session_id) {
            notify.cancel();
        }
    }
}

async fn set_acp_title_if_empty(
    sessions: &AcpSessions,
    session_id: &str,
    prompt_text: &str,
) -> anyhow::Result<()> {
    let mut sessions = sessions.lock().await;
    sessions.set_title_if_empty(session_id, prompt_text)
}

fn acp_session_title(cwd: Option<&Path>) -> Option<String> {
    cwd.and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .map(|name| format!("Zed: {name}"))
}

async fn request_permission(
    cx: &ConnectionTo<Client>,
    session_id: agent_client_protocol::schema::v1::SessionId,
    request: &ToolApprovalRequest,
) -> ToolApprovalDecision {
    let permission = RequestPermissionRequest::new(
        session_id,
        ToolCallUpdate::new(
            request.tool_call_id.clone(),
            ToolCallUpdateFields::new()
                .title(request.tool_name.clone())
                .status(ToolCallStatus::Pending)
                .content(vec![ToolCallContent::from(ContentBlock::Text(
                    TextContent::new(approval_details(request)),
                ))]),
        ),
        vec![
            PermissionOption::new(
                PERMISSION_ALLOW_ONCE,
                "Allow once",
                PermissionOptionKind::AllowOnce,
            ),
            PermissionOption::new(
                PERMISSION_ALLOW_SAME_COMMAND_SESSION,
                "Allow same command this session",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new(
                PERMISSION_ALLOW_RISK_LEVEL_SESSION,
                "Allow similar risk this session",
                PermissionOptionKind::AllowAlways,
            ),
            PermissionOption::new(PERMISSION_DENY, "Deny", PermissionOptionKind::RejectOnce),
        ],
    );
    let response = cx.send_request(permission).block_task().await;
    match response.map(|response| response.outcome) {
        Ok(RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. })) => {
            approval_decision_from_option(option_id.to_string().as_str())
        }
        Ok(RequestPermissionOutcome::Cancelled) | Ok(_) | Err(_) => ToolApprovalDecision::Deny,
    }
}

async fn request_user_input(
    cx: &ConnectionTo<Client>,
    session_id: agent_client_protocol::schema::v1::SessionId,
    request: &UserQuestionRequest,
) -> UserQuestionResponse {
    let mut schema = ElicitationSchema::new()
        .title(Some("User input".to_string()))
        .description(request.reason.clone())
        .string(
            "answer",
            request.allow_free_text || request.options.is_empty(),
        );
    if !request.options.is_empty() {
        schema = schema.string("option", !request.allow_free_text);
    }
    let mut scope = ElicitationSessionScope::new(session_id);
    scope = scope.tool_call_id(Some(ToolCallId::new(request.tool_call_id.clone())));
    let elicitation = CreateElicitationRequest::new(
        ElicitationFormMode::new(scope, schema),
        request.question.clone(),
    );
    match cx.send_request(elicitation).block_task().await {
        Ok(response) => response_to_user_question(request, response.action),
        Err(_) => cancelled_user_question_response(request),
    }
}

fn response_to_user_question(
    request: &UserQuestionRequest,
    action: ElicitationAction,
) -> UserQuestionResponse {
    let ElicitationAction::Accept(ElicitationAcceptAction { content, .. }) = action else {
        return cancelled_user_question_response(request);
    };
    let content = content.unwrap_or_default();
    let free_text = elicitation_string(&content, "answer");
    let selected_option_ids = elicitation_string(&content, "option")
        .into_iter()
        .filter(|id| request.options.iter().any(|option| option.id == *id))
        .collect::<Vec<_>>();
    let answer_text = build_user_question_answer_text(&selected_option_ids, free_text.as_deref());
    UserQuestionResponse {
        question_id: request.id.clone(),
        status: UserQuestionStatus::Answered,
        selected_option_ids,
        free_text,
        answer_text,
        answered_at: None,
        source: Some(ACP_CHANNEL.to_string()),
    }
}

fn elicitation_string(
    content: &BTreeMap<String, ElicitationContentValue>,
    key: &str,
) -> Option<String> {
    match content.get(key) {
        Some(ElicitationContentValue::String(value)) if !value.trim().is_empty() => {
            Some(value.trim().to_string())
        }
        Some(ElicitationContentValue::StringArray(values)) => values
            .iter()
            .find(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_string()),
        _ => None,
    }
}

fn cancelled_user_question_response(request: &UserQuestionRequest) -> UserQuestionResponse {
    UserQuestionResponse {
        question_id: request.id.clone(),
        status: UserQuestionStatus::Cancelled,
        selected_option_ids: Vec::new(),
        free_text: None,
        answer_text: Some("cancelled".to_string()),
        answered_at: None,
        source: Some(ACP_CHANNEL.to_string()),
    }
}

fn build_user_question_answer_text(
    selected_option_ids: &[String],
    free_text: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if !selected_option_ids.is_empty() {
        parts.push(format!("selected: {}", selected_option_ids.join(", ")));
    }
    if let Some(text) = free_text.filter(|text| !text.trim().is_empty()) {
        parts.push(text.trim().to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

fn approval_decision_from_option(option_id: &str) -> ToolApprovalDecision {
    match option_id {
        PERMISSION_ALLOW_ONCE => ToolApprovalDecision::AllowOnce,
        PERMISSION_ALLOW_SAME_COMMAND_SESSION => ToolApprovalDecision::AllowSameCommandSession,
        PERMISSION_ALLOW_RISK_LEVEL_SESSION => ToolApprovalDecision::AllowRiskLevelSession,
        _ => ToolApprovalDecision::Deny,
    }
}

fn approval_details(request: &ToolApprovalRequest) -> String {
    let mut details = format!(
        "Tool: {}\nRisk: {:?}\nArguments: {}",
        request.tool_name, request.risk, request.args_summary
    );
    if let Some(reason) = &request.model_review_reason {
        details.push_str("\nReason: ");
        details.push_str(reason);
    }
    details
}

fn send_content_chunk(
    cx: &ConnectionTo<Client>,
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    update: fn(ContentChunk) -> SessionUpdate,
    message_id: MessageId,
    text: String,
) -> agent_client_protocol::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    cx.send_notification(SessionNotification::new(
        session_id,
        update(
            ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                .message_id(Some(message_id)),
        ),
    ))
}

fn send_tool_call(
    cx: &ConnectionTo<Client>,
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    id: String,
    name: String,
    args: Option<serde_json::Value>,
) -> agent_client_protocol::Result<()> {
    let mut call = ToolCall::new(id, name.clone())
        .kind(tool_kind(&name))
        .status(ToolCallStatus::InProgress);
    if let Some(args) = args {
        call = call.raw_input(args);
    }
    cx.send_notification(SessionNotification::new(
        session_id,
        SessionUpdate::ToolCall(call),
    ))
}

fn send_tool_result(
    cx: &ConnectionTo<Client>,
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    id: String,
    name: String,
    result: String,
    success: bool,
) -> agent_client_protocol::Result<()> {
    let status = if success {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    };
    let fields = ToolCallUpdateFields::new()
        .title(name)
        .status(status)
        .content(vec![ToolCallContent::from(ContentBlock::Text(
            TextContent::new(result.clone()),
        ))])
        .raw_output(serde_json::json!({
            "result": result,
            "success": success,
        }));
    cx.send_notification(SessionNotification::new(
        session_id,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields)),
    ))
}

fn send_tool_input_update(
    cx: &ConnectionTo<Client>,
    session_id: impl Into<agent_client_protocol::schema::v1::SessionId>,
    id: String,
    name: String,
    args: serde_json::Value,
) -> agent_client_protocol::Result<()> {
    let fields = ToolCallUpdateFields::new()
        .title(name)
        .status(ToolCallStatus::InProgress)
        .raw_input(args);
    cx.send_notification(SessionNotification::new(
        session_id,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields)),
    ))
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "fs_read" | "read" | "skill__get" => ToolKind::Read,
        "fs_write" | "fs_create" | "edit" | "apply_patch" => ToolKind::Edit,
        "fs_delete" | "delete" => ToolKind::Delete,
        "search" | "skill__search" | "grep" | "rg" => ToolKind::Search,
        "bash" | "shell" | "exec" | "cmd" => ToolKind::Execute,
        _ => ToolKind::Other,
    }
}

fn prompt_to_text(blocks: Vec<ContentBlock>) -> agent_client_protocol::Result<String> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                if !text.text.trim().is_empty() {
                    parts.push(text.text);
                }
            }
            ContentBlock::ResourceLink(link) => {
                parts.push(format!("{}: {}", link.name, link.uri));
            }
            other => {
                parts.push(format!("[unsupported ACP content block: {other:?}]"));
            }
        }
    }
    let text = parts.join("\n\n");
    if text.trim().is_empty() {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("session/prompt requires at least one text or resource_link content block"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::{
        approval_decision_from_option, create_acp_session, sub_session_event_line,
        sub_session_tool_status, AcpRuntimeFactory, ACP_CWD_METADATA_KEY, PERMISSION_ALLOW_ONCE,
        PERMISSION_DENY,
    };
    use agent_client_protocol::schema::v1::NewSessionRequest;
    use agent_client_protocol::schema::v1::ToolCallStatus;
    use bot_core::ToolApprovalDecision;
    use remi_agentloop::prelude::{ProtocolEvent, RunId, SubSessionEvent, ThreadId};
    use std::rc::Rc;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[test]
    fn permission_option_maps_to_tool_decision() {
        assert_eq!(
            approval_decision_from_option(PERMISSION_ALLOW_ONCE),
            ToolApprovalDecision::AllowOnce
        );
        assert_eq!(
            approval_decision_from_option(PERMISSION_DENY),
            ToolApprovalDecision::Deny
        );
        assert_eq!(
            approval_decision_from_option("unknown"),
            ToolApprovalDecision::Deny
        );
    }

    #[tokio::test]
    async fn runtime_factory_records_and_applies_acp_session_cwd() {
        let data_dir = std::env::temp_dir().join(format!("remi-acp-data-{}", uuid::Uuid::new_v4()));
        let workspace_dir =
            std::env::temp_dir().join(format!("remi-acp-work-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::create_dir_all(&workspace_dir).unwrap();
        unsafe {
            std::env::set_var("REMI_DATA_DIR", &data_dir);
            std::env::remove_var("REMI_SANDBOX_HOST_DIR");
        }
        let sessions = Arc::new(Mutex::new(
            crate::session::SessionRuntime::load(data_dir.clone()).unwrap(),
        ));
        let request = NewSessionRequest::new(&workspace_dir);
        let session_id = create_acp_session(&sessions, "default", &request)
            .await
            .unwrap();
        assert_eq!(
            sessions
                .lock()
                .await
                .metadata_string(&session_id, ACP_CWD_METADATA_KEY)
                .as_deref(),
            Some(workspace_dir.to_str().unwrap())
        );
        let factory = Rc::new(AcpRuntimeFactory::new(
            data_dir,
            Arc::new(Mutex::new(crate::secret_store::SecretStore::from_env())),
            sessions,
            "default".to_string(),
        ));

        factory.configure_workspace_env(&workspace_dir);

        assert_eq!(
            std::env::var_os("REMI_SANDBOX_HOST_DIR").as_deref(),
            Some(workspace_dir.as_os_str())
        );
    }

    #[test]
    fn sub_session_events_map_to_acp_tool_card_status_and_text() {
        let event = SubSessionEvent::new(
            "parent-call",
            ThreadId("child-thread".to_string()),
            RunId("child-run".to_string()),
            "coder",
            Some("Coder Agent".to_string()),
            1,
            ProtocolEvent::ToolCallStart {
                id: "tool-1".to_string(),
                name: "bash".to_string(),
            },
        );

        assert_eq!(
            sub_session_tool_status(event.event.as_ref()),
            ToolCallStatus::InProgress
        );
        assert_eq!(
            sub_session_event_line(&event).as_deref(),
            Some("Tool `bash` started (tool-1).")
        );

        let done = SubSessionEvent::new(
            event.parent_tool_call_id,
            event.sub_thread_id,
            event.sub_run_id,
            event.agent_name,
            event.title,
            event.depth,
            ProtocolEvent::Custom {
                event_type: "sub_session_done".to_string(),
                extra: serde_json::json!({ "final_output": "finished" }),
            },
        );
        assert_eq!(
            sub_session_tool_status(done.event.as_ref()),
            ToolCallStatus::Completed
        );
        assert_eq!(
            sub_session_event_line(&done).as_deref(),
            Some("Done:\nfinished")
        );
    }
}
