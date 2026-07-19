//! Thread-safe embedding API for host applications.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex as StdMutex};
use std::thread::JoinHandle;

use anyhow::{anyhow, Context};
use bot_core::{
    AgentProfile, BuiltinSkill, Content, DynamicTool, ModelProfileConfig, ReasoningEffort,
    SkillSummary, SteerSubmitResult, ThreadHistoryMessage, ToolApprovalDecision,
    ToolApprovalRequest, UserQuestionRequest, UserQuestionResponse, WorkflowDefinition,
};
use futures::StreamExt;
use remi_agentloop::prelude::CancellationToken;
use remi_agentloop::prelude::{ProtocolEvent, SubSessionEvent};
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument as _;

use crate::channel::feishu::LocalImFileBridge;
use crate::core::{ChatChannel, ChatRequest, CoreChatEvent, Runtime};
use crate::secret_store::SecretStore;
#[cfg(test)]
use crate::session::ChannelBinding;
use crate::session::{Session, SessionRuntime, SubSessionKind};
use bot_core::tool_tasks::TOOL_TASK_RUNNING;
use bot_core::{CatBotBuilder, ImFileBridge};
use user_store::UserStore;

const COMMAND_CAPACITY: usize = 128;
const EVENT_CAPACITY: usize = 64;
const SESSION_APP_ID_METADATA_KEY: &str = "application_id";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicationInfo {
    pub app_id: String,
    pub workspace: PathBuf,
    pub data_dir: PathBuf,
    pub telemetry_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRunInfo {
    pub id: u64,
    pub app_id: String,
    pub session_id: String,
    pub state: ActiveRunState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveRunState {
    Running,
    Cancelling,
}

#[derive(Debug, Clone)]
pub struct HookReloadReport {
    pub hooks: Vec<bot_core::HookStatus>,
}

#[derive(Debug, Clone)]
pub struct HookMutationResult {
    pub found: bool,
    pub hook: Option<bot_core::HookStatus>,
}

#[derive(Debug, Clone, Default)]
pub struct ApplicationCatalog {
    pub models: Vec<ModelProfileConfig>,
    pub agents: Vec<AgentProfile>,
    pub skills: Vec<SkillSummary>,
    pub workflows: Vec<WorkflowDefinition>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionPatch {
    pub root_agent_id: Option<String>,
    pub metadata: serde_json::Map<String, serde_json::Value>,
    pub remove_metadata: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EffectiveSessionConfig {
    pub session: Session,
    pub model_profile_id: Option<String>,
    pub agent_id: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub workspace: Option<PathBuf>,
    pub input_history: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum CommandPreprocessResult {
    Reply(String),
    Continue,
}

#[derive(Debug, Clone)]
pub struct SubSessionEventPage {
    pub events: Vec<SubSessionEvent>,
    pub next_offset: u64,
}

#[derive(Debug, Clone)]
pub struct ChannelConfig {
    pub id: String,
    pub sender_user_id: Option<String>,
    pub sender_username: Option<String>,
    pub chat_type: Option<String>,
}

impl ChannelConfig {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            sender_user_id: None,
            sender_username: None,
            chat_type: None,
        }
    }

    pub fn sender(mut self, user_id: impl Into<String>, username: impl Into<String>) -> Self {
        self.sender_user_id = Some(user_id.into());
        self.sender_username = Some(username.into());
        self
    }

    pub fn chat_type(mut self, value: impl Into<String>) -> Self {
        self.chat_type = Some(value.into());
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub model_profile_id: Option<String>,
    pub agent_id: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub async_agent: bool,
}

impl RunOptions {
    pub fn model_profile(mut self, id: impl Into<String>) -> Self {
        self.model_profile_id = Some(id.into());
        self
    }
    pub fn agent(mut self, id: impl Into<String>) -> Self {
        self.agent_id = Some(id.into());
        self
    }
    pub fn reasoning_effort(mut self, value: ReasoningEffort) -> Self {
        self.reasoning_effort = Some(value);
        self
    }
    pub fn async_agent(mut self, value: bool) -> Self {
        self.async_agent = value;
        self
    }
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub session_id: String,
    pub content: Content,
    pub options: RunOptions,
}

impl RunRequest {
    pub fn new(session_id: impl Into<String>, content: Content) -> Self {
        Self {
            session_id: session_id.into(),
            content,
            options: RunOptions::default(),
        }
    }
    pub fn text(session_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(session_id, Content::text(text.into()))
    }
    pub fn options(mut self, options: RunOptions) -> Self {
        self.options = options;
        self
    }
}

#[derive(Debug)]
pub enum ApplicationEvent {
    Prefix(String),
    Reply(String),
    SupervisorStarted,
    Cat(bot_core::CatEvent),
    Done,
}

pub struct RunHandle {
    id: u64,
    app_id: Arc<str>,
    rx: mpsc::Receiver<ApplicationEvent>,
    commands: mpsc::Sender<Command>,
}

#[derive(Clone, Debug)]
pub struct RunControl {
    id: u64,
    app_id: Arc<str>,
    commands: mpsc::Sender<Command>,
}

impl RunControl {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn app_id(&self) -> &str {
        &self.app_id
    }
    pub async fn cancel(&self) -> anyhow::Result<bool> {
        request(&self.commands, |reply| Command::Cancel {
            run_id: self.id,
            force: false,
            reply,
        })
        .await
    }
    pub async fn terminate(&self) -> anyhow::Result<bool> {
        request(&self.commands, |reply| Command::Cancel {
            run_id: self.id,
            force: true,
            reply,
        })
        .await
    }
}

impl RunHandle {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub async fn recv(&mut self) -> Option<ApplicationEvent> {
        self.rx.recv().await
    }
    pub async fn cancel(&self) -> anyhow::Result<bool> {
        request(&self.commands, |reply| Command::Cancel {
            run_id: self.id,
            force: false,
            reply,
        })
        .await
    }
    pub fn control(&self) -> RunControl {
        RunControl {
            id: self.id,
            app_id: self.app_id.clone(),
            commands: self.commands.clone(),
        }
    }
}

pub struct ApplicationBuilder {
    app_id: String,
    data_dir: PathBuf,
    workspace: PathBuf,
    root_agent_id: String,
    default_model: Option<String>,
    model_profiles: Vec<ModelProfileConfig>,
    agent_profiles: Vec<AgentProfile>,
    workflows: Vec<WorkflowDefinition>,
    unsupported_extensions: Vec<String>,
    model_source: Option<PathBuf>,
    tools: Vec<DynamicTool>,
    builtin_skills: Vec<BuiltinSkill>,
    include_default_skills: bool,
    file_skills: bool,
    discover_hooks: bool,
    hook_sources: Vec<bot_core::HookSource>,
    sentry_dsn: Option<String>,
    credentials: Option<std::collections::BTreeMap<String, String>>,
    secret_store: Option<SecretStore>,
}

impl ApplicationBuilder {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            app_id: "remi-cat".into(),
            data_dir: data_dir.into(),
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            root_agent_id: "default".into(),
            default_model: None,
            model_profiles: Vec::new(),
            agent_profiles: Vec::new(),
            workflows: Vec::new(),
            unsupported_extensions: Vec::new(),
            model_source: None,
            tools: Vec::new(),
            builtin_skills: Vec::new(),
            include_default_skills: true,
            file_skills: true,
            discover_hooks: true,
            hook_sources: Vec::new(),
            sentry_dsn: None,
            credentials: None,
            secret_store: None,
        }
    }

    pub fn app_id(mut self, app_id: impl Into<String>) -> Self {
        self.app_id = app_id.into();
        self
    }
    /// Enable application-scoped telemetry using the embedding host's DSN.
    /// Applications that do not call this method never initialize Sentry.
    pub fn sentry_dsn(mut self, dsn: impl Into<String>) -> Self {
        self.sentry_dsn = Some(dsn.into());
        self
    }
    pub fn credentials(mut self, credentials: std::collections::BTreeMap<String, String>) -> Self {
        self.credentials = Some(credentials);
        self
    }
    pub(crate) fn secret_store(mut self, secret_store: SecretStore) -> Self {
        self.secret_store = Some(secret_store);
        self
    }
    pub fn workspace(mut self, workspace: impl Into<PathBuf>) -> Self {
        self.workspace = workspace.into();
        self
    }

    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os("REMI_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".remi-cat")),
        )
    }

    pub fn default_agent(mut self, id: impl Into<String>) -> Self {
        self.root_agent_id = id.into();
        self
    }
    pub fn default_model(mut self, id: impl Into<String>) -> Self {
        self.default_model = Some(id.into());
        self
    }
    pub fn model_profile(mut self, profile: ModelProfileConfig) -> Self {
        self.model_profiles.push(profile);
        self
    }
    pub fn agent_profile(mut self, profile: AgentProfile) -> Self {
        self.agent_profiles.push(profile);
        self
    }
    pub fn workflow(mut self, workflow: WorkflowDefinition) -> Self {
        self.workflows.push(workflow);
        self
    }
    pub fn model_source(mut self, path: impl Into<PathBuf>) -> Self {
        self.model_source = Some(path.into());
        self
    }
    pub fn tool(mut self, tool: DynamicTool) -> Self {
        self.tools.push(tool);
        self
    }
    pub fn builtin_skill(mut self, skill: BuiltinSkill) -> Self {
        self.builtin_skills.push(skill);
        self
    }
    pub fn include_default_skills(mut self, include: bool) -> Self {
        self.include_default_skills = include;
        self
    }
    pub fn file_skills(mut self, include: bool) -> Self {
        self.file_skills = include;
        self
    }
    pub fn discover_hooks(mut self, discover: bool) -> Self {
        self.discover_hooks = discover;
        self
    }
    pub fn hook_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.hook_sources
            .push(bot_core::HookSource::File(path.into()));
        self
    }
    pub fn hook_text(
        mut self,
        id: impl Into<String>,
        format: bot_core::HookTextFormat,
        text: impl Into<String>,
    ) -> Self {
        self.hook_sources.push(bot_core::HookSource::Text {
            id: id.into(),
            format,
            text: text.into(),
        });
        self
    }

    /// Reserved until bot-core exposes registry injection. Fails at `spawn` rather than silently ignoring it.
    pub fn extension(mut self, description: impl Into<String>) -> Self {
        self.unsupported_extensions.push(description.into());
        self
    }

    pub async fn spawn(self) -> anyhow::Result<Application> {
        let app_id = self.app_id.trim().to_string();
        validate_app_id(&app_id)?;
        let telemetry = match self.sentry_dsn.as_deref() {
            None => None,
            Some(dsn) if dsn.trim().is_empty() => {
                return Err(anyhow!("Application Sentry DSN must not be empty"));
            }
            Some(dsn) => Some(Arc::new(crate::telemetry::ApplicationTelemetry::new(
                dsn.trim(),
                &app_id,
            )?)),
        };
        if !self.unsupported_extensions.is_empty() {
            return Err(anyhow!(
                "custom extension registration is not supported by this bot-core build: {}",
                self.unsupported_extensions.join(", ")
            ));
        }
        let (tx, rx) = mpsc::channel(COMMAND_CAPACITY);
        let (ready_tx, ready_rx) = oneshot::channel();
        let info = Arc::new(ApplicationInfo {
            app_id: app_id.clone(),
            workspace: self.workspace.clone(),
            data_dir: self.data_dir.clone(),
            telemetry_enabled: telemetry.is_some(),
        });
        let data_dir = self.data_dir;
        let workspace = self.workspace;
        let root_agent_id = self.root_agent_id;
        let default_model = self.default_model;
        let model_source = self.model_source;
        let tools = self.tools;
        let builtin_skills = self.builtin_skills;
        let include_default_skills = self.include_default_skills;
        let file_skills = self.file_skills;
        let discover_hooks = self.discover_hooks;
        let hook_sources = self.hook_sources;
        let credentials = self.credentials;
        let secret_store = self.secret_store;
        let thread_telemetry = telemetry.clone();
        let mut text_ids = std::collections::HashSet::new();
        for source in &hook_sources {
            if let bot_core::HookSource::Text { id, .. } = source {
                if id.trim().is_empty() || !text_ids.insert(id.clone()) {
                    return Err(anyhow!(
                        "hook text ids must be non-empty and unique: `{id}`"
                    ));
                }
            }
        }
        if model_source.is_none() {
            bot_core::install_embedded_model_profiles(data_dir.join("models"))?;
        }
        install_extensions(
            &data_dir,
            &self.model_profiles,
            &self.agent_profiles,
            &self.workflows,
        )?;
        let thread_tx = tx.clone();
        let thread = std::thread::Builder::new()
            .name("remi-cat-application".into())
            .spawn(move || {
                let execute_telemetry = thread_telemetry.clone();
                let execute = || {
                    let runtime = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            if let Some(telemetry) = &execute_telemetry {
                                telemetry
                                    .capture_system_error(error.to_string(), "application.runtime");
                            }
                            let _ = ready_tx.send(Err(anyhow!(error)));
                            return;
                        }
                    };
                    let local = tokio::task::LocalSet::new();
                    local.block_on(&runtime, async move {
                        match build_runtime(
                            data_dir,
                            workspace,
                            root_agent_id,
                            default_model,
                            model_source,
                            tools,
                            builtin_skills,
                            include_default_skills,
                            file_skills,
                            discover_hooks,
                            hook_sources,
                            credentials,
                            secret_store,
                        )
                        .await
                        {
                            Ok(runtime) => {
                                let _ = ready_tx.send(Ok(()));
                                dispatch(
                                    runtime,
                                    rx,
                                    thread_tx,
                                    Arc::from(app_id),
                                    execute_telemetry.clone(),
                                )
                                .await;
                            }
                            Err(error) => {
                                if let Some(telemetry) = &execute_telemetry {
                                    telemetry.capture_system_error(
                                        error.to_string(),
                                        "application.startup",
                                    );
                                }
                                let _ = ready_tx.send(Err(error));
                            }
                        }
                    });
                };
                if let Some(telemetry) = &thread_telemetry {
                    telemetry.run(execute);
                    telemetry.flush();
                } else {
                    execute();
                }
            })
            .context("spawning remi-cat application thread")?;
        ready_rx
            .await
            .context("application thread exited during startup")??;
        let handle = ApplicationHandle { commands: tx, info };
        Ok(Application {
            handle,
            thread: Arc::new(StdMutex::new(Some(thread))),
        })
    }
}

pub struct Application {
    handle: ApplicationHandle,
    thread: Arc<StdMutex<Option<JoinHandle<()>>>>,
}

impl Application {
    pub fn handle(&self) -> ApplicationHandle {
        self.handle.clone()
    }
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let _ = request(&self.handle.commands, Command::Shutdown).await;
        let thread = self
            .thread
            .lock()
            .expect("application thread mutex poisoned")
            .take();
        if let Some(thread) = thread {
            tokio::task::spawn_blocking(move || thread.join())
                .await
                .context("joining application thread")?
                .map_err(|_| anyhow!("application thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for Application {
    fn drop(&mut self) {
        let (reply, _) = oneshot::channel();
        let _ = self.handle.commands.try_send(Command::Shutdown(reply));
    }
}

#[derive(Clone)]
pub struct ApplicationHandle {
    commands: mpsc::Sender<Command>,
    info: Arc<ApplicationInfo>,
}

impl ApplicationHandle {
    pub fn app_id(&self) -> &str {
        &self.info.app_id
    }
    pub fn info(&self) -> ApplicationInfo {
        (*self.info).clone()
    }
    pub fn channel(&self, config: ChannelConfig) -> ChannelHandle {
        ChannelHandle {
            commands: self.commands.clone(),
            config,
            info: self.info.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ChannelHandle {
    commands: mpsc::Sender<Command>,
    config: ChannelConfig,
    info: Arc<ApplicationInfo>,
}

impl ChannelHandle {
    pub fn app_id(&self) -> &str {
        &self.info.app_id
    }
    pub fn application_info(&self) -> ApplicationInfo {
        (*self.info).clone()
    }
    pub async fn resolve_session(&self, channel_id: impl Into<String>) -> anyhow::Result<Session> {
        request(&self.commands, |reply| Command::ResolveSession {
            platform: self.config.id.clone(),
            channel_id: channel_id.into(),
            reply,
        })
        .await
    }
    pub async fn sessions(&self) -> anyhow::Result<Vec<Session>> {
        request(&self.commands, |reply| Command::ListSessions { reply }).await
    }
    pub async fn session(&self, id: impl Into<String>) -> anyhow::Result<Option<Session>> {
        request(&self.commands, |reply| Command::GetSession {
            id: id.into(),
            reply,
        })
        .await
    }
    pub async fn catalog(&self) -> anyhow::Result<ApplicationCatalog> {
        request(&self.commands, |reply| Command::Catalog { reply }).await
    }
    pub async fn patch_session(
        &self,
        id: impl Into<String>,
        patch: SessionPatch,
    ) -> anyhow::Result<Option<Session>> {
        request(&self.commands, |reply| Command::PatchSession {
            id: id.into(),
            patch,
            reply,
        })
        .await
    }
    pub async fn effective_session_config(
        &self,
        id: impl Into<String>,
    ) -> anyhow::Result<Option<EffectiveSessionConfig>> {
        let Some(session) = self.session(id).await? else {
            return Ok(None);
        };
        let model_profile_id = session
            .metadata
            .get(crate::SESSION_MODEL_PROFILE_METADATA_KEY)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let agent_id = session
            .metadata
            .get(crate::SESSION_AGENT_ID_METADATA_KEY)
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&session.root_agent_id)
            .to_string();
        let reasoning_effort = session
            .metadata
            .get(crate::app::SESSION_REASONING_EFFORT_METADATA_KEY)
            .and_then(serde_json::Value::as_str)
            .and_then(ReasoningEffort::parse);
        let workspace = session
            .metadata
            .get("tui_workspace_dir")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from);
        let input_history = session
            .metadata
            .get(crate::SESSION_INPUT_HISTORY_METADATA_KEY)
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        Ok(Some(EffectiveSessionConfig {
            session,
            model_profile_id,
            agent_id,
            reasoning_effort,
            workspace,
            input_history,
        }))
    }
    pub async fn compact(&self, id: impl Into<String>) -> anyhow::Result<usize> {
        request(&self.commands, |reply| Command::Compact {
            id: id.into(),
            reply,
        })
        .await
    }
    pub async fn decide_approval(
        &self,
        id: impl Into<String>,
        decision: ToolApprovalDecision,
    ) -> anyhow::Result<Option<ToolApprovalRequest>> {
        request(&self.commands, |reply| Command::DecideApproval {
            id: id.into(),
            decision,
            reply,
        })
        .await
    }
    pub async fn answer_user_question(
        &self,
        id: impl Into<String>,
        response: UserQuestionResponse,
    ) -> anyhow::Result<Option<UserQuestionRequest>> {
        request(&self.commands, |reply| Command::AnswerQuestion {
            id: id.into(),
            response,
            reply,
        })
        .await
    }
    pub async fn cancel_user_question(
        &self,
        id: impl Into<String>,
        source: Option<String>,
    ) -> anyhow::Result<Option<UserQuestionRequest>> {
        request(&self.commands, |reply| Command::CancelQuestion {
            id: id.into(),
            source,
            reply,
        })
        .await
    }
    pub async fn record_sub_session(
        &self,
        parent_id: impl Into<String>,
        event: SubSessionEvent,
    ) -> anyhow::Result<String> {
        request(&self.commands, |reply| Command::RecordSubSession {
            parent_id: parent_id.into(),
            event,
            config: self.config.clone(),
            reply,
        })
        .await
    }
    pub async fn preprocess_command(
        &self,
        session_id: impl Into<String>,
        command: impl Into<String>,
    ) -> anyhow::Result<CommandPreprocessResult> {
        request(&self.commands, |reply| Command::PreprocessCommand {
            session_id: session_id.into(),
            command: command.into(),
            reply,
        })
        .await
    }
    pub async fn background_tasks(
        &self,
        session_id: impl Into<String>,
    ) -> anyhow::Result<Vec<bot_core::ToolTaskRecord>> {
        request(&self.commands, |reply| Command::BackgroundTasks {
            session_id: session_id.into(),
            reply,
        })
        .await
    }
    pub async fn background_task(
        &self,
        task_id: impl Into<String>,
    ) -> anyhow::Result<Option<bot_core::ToolTaskRecord>> {
        request(&self.commands, |reply| Command::BackgroundTask {
            task_id: task_id.into(),
            reply,
        })
        .await
    }
    pub async fn cancel_background_task(
        &self,
        task_id: impl Into<String>,
    ) -> anyhow::Result<Option<bot_core::ToolTaskRecord>> {
        request(&self.commands, |reply| Command::CancelBackgroundTask {
            task_id: task_id.into(),
            reply,
        })
        .await
    }
    pub async fn wait_background_task(
        &self,
        task_id: impl Into<String>,
    ) -> anyhow::Result<Option<bot_core::ToolTaskRecord>> {
        request(&self.commands, |reply| Command::WaitBackgroundTask {
            task_id: task_id.into(),
            reply,
        })
        .await
    }
    pub async fn todo(&self, session_id: impl Into<String>) -> anyhow::Result<Option<String>> {
        request(&self.commands, |reply| Command::Todo {
            session_id: session_id.into(),
            reply,
        })
        .await
    }
    pub async fn workflow_status(
        &self,
        session_id: impl Into<String>,
    ) -> anyhow::Result<Option<bot_core::WorkflowInstance>> {
        request(&self.commands, |reply| Command::WorkflowStatus {
            session_id: session_id.into(),
            reply,
        })
        .await
    }
    pub async fn start_workflow(
        &self,
        session_id: impl Into<String>,
        workflow_id: impl Into<String>,
        context: serde_json::Value,
        max_rounds: bot_core::WorkflowMaxRounds,
    ) -> anyhow::Result<bot_core::WorkflowInstance> {
        request(&self.commands, |reply| Command::StartWorkflow {
            session_id: session_id.into(),
            workflow_id: workflow_id.into(),
            context,
            max_rounds,
            reply,
        })
        .await
    }
    pub async fn pause_workflow(&self, session_id: impl Into<String>) -> anyhow::Result<()> {
        request(&self.commands, |reply| Command::PauseWorkflow {
            session_id: session_id.into(),
            reply,
        })
        .await
    }
    pub async fn stop_workflow(&self, session_id: impl Into<String>) -> anyhow::Result<()> {
        request(&self.commands, |reply| Command::StopWorkflow {
            session_id: session_id.into(),
            reply,
        })
        .await
    }
    pub async fn model_inputs(
        &self,
        session_id: impl Into<String>,
        limit: Option<usize>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        request(&self.commands, |reply| Command::ModelInputs {
            session_id: session_id.into(),
            limit,
            reply,
        })
        .await
    }
    pub async fn sub_sessions(
        &self,
        parent_id: impl Into<String>,
    ) -> anyhow::Result<Vec<crate::session::SubSession>> {
        Ok(self
            .session(parent_id)
            .await?
            .map(|session| session.sub_sessions)
            .unwrap_or_default())
    }
    pub async fn sub_session_events(
        &self,
        session_id: impl Into<String>,
        offset: u64,
        limit: usize,
    ) -> anyhow::Result<SubSessionEventPage> {
        request(&self.commands, |reply| Command::SubSessionEvents {
            session_id: session_id.into(),
            offset,
            limit,
            reply,
        })
        .await
    }
    pub async fn steer_sub_session(
        &self,
        thread_id: impl Into<String>,
        content: Content,
    ) -> anyhow::Result<SteerSubmitResult> {
        request(&self.commands, |reply| Command::SteerSubSession {
            thread_id: thread_id.into(),
            content,
            config: self.config.clone(),
            reply,
        })
        .await
    }
    pub async fn cancel_sub_session(&self, thread_id: impl Into<String>) -> anyhow::Result<bool> {
        request(&self.commands, |reply| Command::CancelSubSession {
            thread_id: thread_id.into(),
            reply,
        })
        .await
    }
    pub async fn rename_session(
        &self,
        id: impl Into<String>,
        title: impl Into<String>,
    ) -> anyhow::Result<Option<Session>> {
        request(&self.commands, |reply| Command::RenameSession {
            id: id.into(),
            title: title.into(),
            reply,
        })
        .await
    }
    pub async fn fork_session(
        &self,
        source_id: impl Into<String>,
        channel_id: impl Into<String>,
        title: Option<String>,
    ) -> anyhow::Result<Option<Session>> {
        request(&self.commands, |reply| Command::ForkSession {
            source_id: source_id.into(),
            platform: self.config.id.clone(),
            channel_id: channel_id.into(),
            title,
            user_id: self.config.sender_user_id.clone(),
            reply,
        })
        .await
    }
    pub async fn delete_session(&self, id: impl Into<String>) -> anyhow::Result<Option<Session>> {
        request(&self.commands, |reply| Command::DeleteSession {
            id: id.into(),
            reply,
        })
        .await
    }
    pub async fn history(
        &self,
        id: impl Into<String>,
    ) -> anyhow::Result<Vec<ThreadHistoryMessage>> {
        request(&self.commands, |reply| Command::History {
            id: id.into(),
            reply,
        })
        .await
    }
    pub async fn run(&self, request_: RunRequest) -> anyhow::Result<RunHandle> {
        let config = self.config.clone();
        request(&self.commands, |reply| Command::Run {
            request: request_,
            config,
            reply,
        })
        .await
    }
    pub async fn steer(&self, request_: RunRequest) -> anyhow::Result<SteerSubmitResult> {
        self.submit(request_, false).await
    }
    pub async fn next_turn(&self, request_: RunRequest) -> anyhow::Result<SteerSubmitResult> {
        self.submit(request_, true).await
    }
    async fn submit(&self, request_: RunRequest, next: bool) -> anyhow::Result<SteerSubmitResult> {
        request(&self.commands, |reply| Command::Submit {
            request: request_,
            config: self.config.clone(),
            next,
            reply,
        })
        .await
    }
    pub async fn cancel(&self, run_id: u64) -> anyhow::Result<bool> {
        request(&self.commands, |reply| Command::Cancel {
            run_id,
            force: false,
            reply,
        })
        .await
    }
    pub async fn active_runs(&self) -> anyhow::Result<Vec<ActiveRunInfo>> {
        request(&self.commands, |reply| Command::ActiveRuns { reply }).await
    }
    pub async fn hooks(&self) -> anyhow::Result<Vec<bot_core::HookStatus>> {
        request(&self.commands, |reply| Command::Hooks { reply }).await
    }
    pub async fn set_hook_trusted(
        &self,
        hash: impl Into<String>,
        trusted: bool,
    ) -> anyhow::Result<HookMutationResult> {
        let hash = hash.into();
        let found = request(&self.commands, |reply| Command::SetHookTrusted {
            hash: hash.clone(),
            trusted,
            reply,
        })
        .await?;
        let hook = if found {
            self.hooks()
                .await?
                .into_iter()
                .find(|hook| hook.hash == hash)
        } else {
            None
        };
        Ok(HookMutationResult { found, hook })
    }
    pub async fn set_hook_enabled(
        &self,
        hash: impl Into<String>,
        enabled: bool,
    ) -> anyhow::Result<HookMutationResult> {
        let hash = hash.into();
        let found = request(&self.commands, |reply| Command::SetHookEnabled {
            hash: hash.clone(),
            enabled,
            reply,
        })
        .await?;
        let hook = if found {
            self.hooks()
                .await?
                .into_iter()
                .find(|hook| hook.hash == hash)
        } else {
            None
        };
        Ok(HookMutationResult { found, hook })
    }
    pub async fn reload_hooks(&self) -> anyhow::Result<HookReloadReport> {
        request(&self.commands, |reply| Command::ReloadHooks { reply }).await
    }
}

enum Command {
    Catalog {
        reply: oneshot::Sender<anyhow::Result<ApplicationCatalog>>,
    },
    PatchSession {
        id: String,
        patch: SessionPatch,
        reply: oneshot::Sender<anyhow::Result<Option<Session>>>,
    },
    Compact {
        id: String,
        reply: oneshot::Sender<anyhow::Result<usize>>,
    },
    DecideApproval {
        id: String,
        decision: ToolApprovalDecision,
        reply: oneshot::Sender<anyhow::Result<Option<ToolApprovalRequest>>>,
    },
    AnswerQuestion {
        id: String,
        response: UserQuestionResponse,
        reply: oneshot::Sender<anyhow::Result<Option<UserQuestionRequest>>>,
    },
    CancelQuestion {
        id: String,
        source: Option<String>,
        reply: oneshot::Sender<anyhow::Result<Option<UserQuestionRequest>>>,
    },
    RecordSubSession {
        parent_id: String,
        event: SubSessionEvent,
        config: ChannelConfig,
        reply: oneshot::Sender<anyhow::Result<String>>,
    },
    PreprocessCommand {
        session_id: String,
        command: String,
        reply: oneshot::Sender<anyhow::Result<CommandPreprocessResult>>,
    },
    BackgroundTasks {
        session_id: String,
        reply: oneshot::Sender<anyhow::Result<Vec<bot_core::ToolTaskRecord>>>,
    },
    BackgroundTask {
        task_id: String,
        reply: oneshot::Sender<anyhow::Result<Option<bot_core::ToolTaskRecord>>>,
    },
    CancelBackgroundTask {
        task_id: String,
        reply: oneshot::Sender<anyhow::Result<Option<bot_core::ToolTaskRecord>>>,
    },
    WaitBackgroundTask {
        task_id: String,
        reply: oneshot::Sender<anyhow::Result<Option<bot_core::ToolTaskRecord>>>,
    },
    Todo {
        session_id: String,
        reply: oneshot::Sender<anyhow::Result<Option<String>>>,
    },
    WorkflowStatus {
        session_id: String,
        reply: oneshot::Sender<anyhow::Result<Option<bot_core::WorkflowInstance>>>,
    },
    StartWorkflow {
        session_id: String,
        workflow_id: String,
        context: serde_json::Value,
        max_rounds: bot_core::WorkflowMaxRounds,
        reply: oneshot::Sender<anyhow::Result<bot_core::WorkflowInstance>>,
    },
    PauseWorkflow {
        session_id: String,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    StopWorkflow {
        session_id: String,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    ModelInputs {
        session_id: String,
        limit: Option<usize>,
        reply: oneshot::Sender<anyhow::Result<Vec<serde_json::Value>>>,
    },
    SubSessionEvents {
        session_id: String,
        offset: u64,
        limit: usize,
        reply: oneshot::Sender<anyhow::Result<SubSessionEventPage>>,
    },
    SteerSubSession {
        thread_id: String,
        content: Content,
        config: ChannelConfig,
        reply: oneshot::Sender<anyhow::Result<SteerSubmitResult>>,
    },
    CancelSubSession {
        thread_id: String,
        reply: oneshot::Sender<anyhow::Result<bool>>,
    },
    ResolveSession {
        platform: String,
        channel_id: String,
        reply: oneshot::Sender<anyhow::Result<Session>>,
    },
    ListSessions {
        reply: oneshot::Sender<anyhow::Result<Vec<Session>>>,
    },
    GetSession {
        id: String,
        reply: oneshot::Sender<anyhow::Result<Option<Session>>>,
    },
    RenameSession {
        id: String,
        title: String,
        reply: oneshot::Sender<anyhow::Result<Option<Session>>>,
    },
    ForkSession {
        source_id: String,
        platform: String,
        channel_id: String,
        title: Option<String>,
        user_id: Option<String>,
        reply: oneshot::Sender<anyhow::Result<Option<Session>>>,
    },
    DeleteSession {
        id: String,
        reply: oneshot::Sender<anyhow::Result<Option<Session>>>,
    },
    History {
        id: String,
        reply: oneshot::Sender<anyhow::Result<Vec<ThreadHistoryMessage>>>,
    },
    Run {
        request: RunRequest,
        config: ChannelConfig,
        reply: oneshot::Sender<anyhow::Result<RunHandle>>,
    },
    Submit {
        request: RunRequest,
        config: ChannelConfig,
        next: bool,
        reply: oneshot::Sender<anyhow::Result<SteerSubmitResult>>,
    },
    Cancel {
        run_id: u64,
        force: bool,
        reply: oneshot::Sender<anyhow::Result<bool>>,
    },
    ActiveRuns {
        reply: oneshot::Sender<anyhow::Result<Vec<ActiveRunInfo>>>,
    },
    Hooks {
        reply: oneshot::Sender<anyhow::Result<Vec<bot_core::HookStatus>>>,
    },
    SetHookTrusted {
        hash: String,
        trusted: bool,
        reply: oneshot::Sender<anyhow::Result<bool>>,
    },
    SetHookEnabled {
        hash: String,
        enabled: bool,
        reply: oneshot::Sender<anyhow::Result<bool>>,
    },
    ReloadHooks {
        reply: oneshot::Sender<anyhow::Result<HookReloadReport>>,
    },
    RunFinished(u64),
    Shutdown(oneshot::Sender<anyhow::Result<()>>),
}

struct ActiveRun {
    session_id: String,
    cancel: CancellationToken,
    stop_delivery: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

async fn request<T>(
    tx: &mpsc::Sender<Command>,
    make: impl FnOnce(oneshot::Sender<anyhow::Result<T>>) -> Command,
) -> anyhow::Result<T> {
    let (reply, rx) = oneshot::channel();
    tx.send(make(reply))
        .await
        .map_err(|_| anyhow!("application is shut down"))?;
    rx.await
        .map_err(|_| anyhow!("application command was abandoned"))?
}

fn install_extensions(
    data_dir: &std::path::Path,
    models: &[ModelProfileConfig],
    agents: &[AgentProfile],
    workflows: &[WorkflowDefinition],
) -> anyhow::Result<()> {
    let mut ids = std::collections::HashSet::new();
    for model in models {
        model.validate()?;
        validate_id(&model.id)?;
        if !ids.insert(("model", model.id.as_str())) {
            return Err(anyhow!("duplicate model profile `{}`", model.id));
        }
        let dir = data_dir.join("models");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join(format!("{}.yaml", model.id)),
            serde_yaml::to_string(model)?,
        )?;
    }
    for agent in agents {
        validate_id(&agent.id)?;
        if !ids.insert(("agent", agent.id.as_str())) {
            return Err(anyhow!("duplicate agent profile `{}`", agent.id));
        }
        let dir = data_dir.join("agents");
        std::fs::create_dir_all(&dir)?;
        let frontmatter = serde_yaml::to_string(agent)?;
        std::fs::write(
            dir.join(format!("{}.md", agent.id)),
            format!("---\n{frontmatter}---\n\n{}\n", agent.system_prompt),
        )?;
    }
    for workflow in workflows {
        validate_id(&workflow.id)?;
        workflow
            .validate()
            .map_err(|error| anyhow!("invalid workflow `{}`: {error}", workflow.id))?;
        if !ids.insert(("workflow", workflow.id.as_str())) {
            return Err(anyhow!("duplicate workflow `{}`", workflow.id));
        }
        let dir = data_dir.join("workflows");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join(format!("{}.json", workflow.id)),
            serde_json::to_vec_pretty(workflow)?,
        )?;
    }
    Ok(())
}

fn validate_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(anyhow!(
            "extension id `{id}` must contain only ASCII letters, digits, `_`, or `-`"
        ));
    }
    Ok(())
}

fn validate_app_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
    {
        return Err(anyhow!(
            "app id must be 1-64 ASCII letters, digits, `.`, `_`, or `-`"
        ));
    }
    Ok(())
}

async fn build_runtime(
    data_dir: PathBuf,
    workspace: PathBuf,
    root_agent_id: String,
    default_model: Option<String>,
    model_source: Option<PathBuf>,
    tools: Vec<DynamicTool>,
    builtin_skills: Vec<BuiltinSkill>,
    include_default_skills: bool,
    file_skills: bool,
    discover_hooks: bool,
    hook_sources: Vec<bot_core::HookSource>,
    credentials: Option<std::collections::BTreeMap<String, String>>,
    configured_secret_store: Option<SecretStore>,
) -> anyhow::Result<Rc<Runtime>> {
    std::fs::create_dir_all(&data_dir)?;
    let bridge: Arc<dyn ImFileBridge> = Arc::new(LocalImFileBridge::new(None));
    let source = model_source.as_ref().unwrap_or(&data_dir);
    let secrets =
        configured_secret_store.unwrap_or_else(|| SecretStore::dotenv(source.join(".env")));
    let secret_entries = match credentials {
        Some(credentials) => credentials,
        None => secrets.entries()?,
    };
    let selected = default_model.or_else(|| {
        crate::runtime_config::load_runtime_config(source)
            .ok()
            .flatten()
            .map(|config| config.model_profile)
    });
    let hook_manager = bot_core::HookManager::with_sources(
        workspace.clone(),
        data_dir.clone(),
        discover_hooks,
        hook_sources,
    )
    .context("loading application hook sources")?;
    let mut builder = CatBotBuilder::from_model_source_with_api_keys(
        &data_dir,
        source,
        selected.as_deref(),
        &secret_entries,
    )?
    .workspace(workspace)
    .hook_manager(hook_manager);
    let agents = bot_core::AgentRegistry::load(data_dir.join("agents"))?;
    if let Some(profile) = agents.get(&root_agent_id) {
        builder = builder.agent_profile(profile.clone())?;
    }
    builder = builder
        .im_bridge(bridge.clone())
        .include_default_agents(false)
        .include_default_skills(include_default_skills)
        .file_skills(file_skills);
    for tool in tools {
        builder = builder.tool(tool);
    }
    for skill in builtin_skills {
        builder = builder.builtin_skill(skill);
    }
    let bot = Rc::new(builder.build()?);
    let secret_store = Arc::new(tokio::sync::Mutex::new(secrets));
    Ok(Rc::new(Runtime {
        bot,
        secret_store,
        user_store: Arc::new(UserStore::load(data_dir.join("users.json"))?),
        sessions: Arc::new(tokio::sync::Mutex::new(SessionRuntime::load(
            data_dir.clone(),
        )?)),
        im_bridge: bridge,
        root_agent_id,
        data_dir,
    }))
}

fn chat_request(
    request: RunRequest,
    config: ChannelConfig,
    cancel: Option<CancellationToken>,
    app_id: &str,
) -> ChatRequest {
    let mut result = ChatRequest::text(request.session_id, ChatChannel::Cli, "")
        .with_content(request.content)
        .with_platform(Some(config.id))
        .with_app_id(app_id);
    if let Some(user_id) = config.sender_user_id {
        result = result.with_sender(user_id, config.sender_username);
    }
    if let Some(chat_type) = config.chat_type {
        result = result.with_message(uuid::Uuid::new_v4().to_string(), chat_type);
    }
    result = result.with_runtime_overrides(
        request.options.model_profile_id,
        request.options.reasoning_effort,
        request.options.agent_id,
        Some(request.options.async_agent),
    );
    if let Some(cancel) = cancel {
        result = result.with_cancel(cancel);
    }
    result
}

async fn dispatch(
    runtime: Rc<Runtime>,
    mut commands: mpsc::Receiver<Command>,
    command_tx: mpsc::Sender<Command>,
    app_id: Arc<str>,
    telemetry: Option<Arc<crate::telemetry::ApplicationTelemetry>>,
) {
    let (internal_tx, mut internal_rx) = mpsc::unbounded_channel();
    let mut runs: HashMap<u64, ActiveRun> = HashMap::new();
    let mut next_id = 1_u64;
    loop {
        let command = tokio::select! { value = commands.recv() => value, value = internal_rx.recv() => value };
        let Some(command) = command else { break };
        match command {
            Command::Catalog { reply } => {
                let value = runtime
                    .bot
                    .workflow_definitions()
                    .map(|workflows| ApplicationCatalog {
                        models: runtime.bot.model_profiles().into_iter().cloned().collect(),
                        agents: runtime.bot.agent_profiles().into_iter().cloned().collect(),
                        skills: runtime.bot.skill_summaries(),
                        workflows,
                    })
                    .map_err(anyhow::Error::from);
                let _ = reply.send(value);
            }
            Command::PatchSession { id, patch, reply } => {
                let mut metadata = patch.metadata;
                metadata.insert(
                    SESSION_APP_ID_METADATA_KEY.into(),
                    serde_json::Value::String(app_id.to_string()),
                );
                let value = runtime.sessions.lock().await.patch(
                    &id,
                    patch.root_agent_id.as_deref(),
                    metadata,
                    &patch.remove_metadata,
                );
                let _ = reply.send(value);
            }
            Command::Compact { id, reply } => {
                let bot = runtime.bot.clone();
                let app_id = app_id.clone();
                tokio::task::spawn_local(async move {
                    let value = bot
                        .compact_memory_for_app(&id, &app_id)
                        .await
                        .map_err(anyhow::Error::from);
                    let _ = reply.send(value);
                });
            }
            Command::DecideApproval {
                id,
                decision,
                reply,
            } => {
                let value = runtime.bot.approval_manager().decide(&id, decision).await;
                let _ = reply.send(Ok(value));
            }
            Command::AnswerQuestion {
                id,
                response,
                reply,
            } => {
                let value = runtime.bot.answer_user_question(&id, response).await;
                let _ = reply.send(Ok(value));
            }
            Command::CancelQuestion { id, source, reply } => {
                let value = runtime
                    .bot
                    .user_question_manager()
                    .cancel(&id, source)
                    .await;
                let _ = reply.send(Ok(value));
            }
            Command::RecordSubSession {
                parent_id,
                event,
                config,
                reply,
            } => {
                let runtime = runtime.clone();
                let app_id = app_id.clone();
                tokio::task::spawn_local(async move {
                    let value =
                        record_sub_session(&runtime, &parent_id, &event, &app_id, &config).await;
                    let _ = reply.send(value);
                });
            }
            Command::PreprocessCommand {
                session_id,
                command,
                reply,
            } => {
                let runtime = runtime.clone();
                tokio::task::spawn_local(async move {
                    let value =
                        crate::command::process_runtime_commands(&runtime, &session_id, &command)
                            .await
                            .map(|result| match result {
                                crate::command::RuntimeCommandPipelineResult::Reply(text) => {
                                    CommandPreprocessResult::Reply(text)
                                }
                                crate::command::RuntimeCommandPipelineResult::StartWorkflow {
                                    ..
                                }
                                | crate::command::RuntimeCommandPipelineResult::Continue {
                                    ..
                                } => CommandPreprocessResult::Continue,
                            });
                    let _ = reply.send(value);
                });
            }
            Command::BackgroundTasks { session_id, reply } => {
                let value = runtime
                    .bot
                    .tool_task_manager()
                    .list_session_background(&session_id)
                    .await;
                let _ = reply.send(Ok(value));
            }
            Command::BackgroundTask { task_id, reply } => {
                let value = runtime.bot.tool_task_manager().get(&task_id).await;
                let _ = reply.send(Ok(value));
            }
            Command::CancelBackgroundTask { task_id, reply } => {
                let value = runtime.bot.tool_task_manager().cancel(&task_id).await;
                let _ = reply.send(Ok(value));
            }
            Command::WaitBackgroundTask { task_id, reply } => {
                let manager = runtime.bot.tool_task_manager().clone();
                tokio::task::spawn_local(async move {
                    let Some(current) = manager.get(&task_id).await else {
                        let _ = reply.send(Ok(None));
                        return;
                    };
                    if current.status != TOOL_TASK_RUNNING {
                        let _ = reply.send(Ok(Some(current)));
                        return;
                    }
                    let mut completed = manager.subscribe_completed(&current.thread_id).await;
                    if let Some(latest) = manager.get(&task_id).await {
                        if latest.status != TOOL_TASK_RUNNING {
                            let _ = reply.send(Ok(Some(latest)));
                            return;
                        }
                    }
                    loop {
                        match completed.recv().await {
                            Ok(record) if record.task_id == task_id => {
                                let _ = reply.send(Ok(Some(record)));
                                return;
                            }
                            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                let _ = reply.send(Ok(manager.get(&task_id).await));
                                return;
                            }
                        }
                    }
                });
            }
            Command::Todo { session_id, reply } => {
                let _ = reply.send(Ok(runtime.bot.todo_card(&session_id).await));
            }
            Command::WorkflowStatus { session_id, reply } => {
                let _ = reply.send(Ok(runtime.bot.workflow_status(&session_id).await));
            }
            Command::StartWorkflow {
                session_id,
                workflow_id,
                context,
                max_rounds,
                reply,
            } => {
                let bot = runtime.bot.clone();
                tokio::task::spawn_local(async move {
                    let value = bot
                        .start_workflow_by_id(&session_id, &workflow_id, context, max_rounds)
                        .await
                        .map_err(anyhow::Error::from);
                    let _ = reply.send(value);
                });
            }
            Command::PauseWorkflow { session_id, reply } => {
                let bot = runtime.bot.clone();
                tokio::task::spawn_local(async move {
                    let value = bot
                        .pause_workflow(&session_id)
                        .await
                        .map_err(anyhow::Error::from);
                    let _ = reply.send(value);
                });
            }
            Command::StopWorkflow { session_id, reply } => {
                let bot = runtime.bot.clone();
                tokio::task::spawn_local(async move {
                    let value = bot
                        .stop_workflow(&session_id)
                        .await
                        .map_err(anyhow::Error::from);
                    let _ = reply.send(value);
                });
            }
            Command::ModelInputs {
                session_id,
                limit,
                reply,
            } => {
                let data_dir = runtime.data_dir.clone();
                tokio::task::spawn_blocking(move || {
                    let value = crate::model_input_store::list_model_input_snapshot_json(
                        &data_dir,
                        &session_id,
                        limit,
                    )
                    .and_then(|items| {
                        items
                            .into_iter()
                            .map(|json| serde_json::from_str(&json).map_err(anyhow::Error::from))
                            .collect()
                    });
                    let _ = reply.send(value);
                });
            }
            Command::SubSessionEvents {
                session_id,
                offset,
                limit,
                reply,
            } => {
                let data_dir = runtime.data_dir.clone();
                tokio::task::spawn_local(async move {
                    let value =
                        read_sub_session_events(&data_dir, &session_id, offset, limit).await;
                    let _ = reply.send(value);
                });
            }
            Command::SteerSubSession {
                thread_id,
                content,
                config,
                reply,
            } => {
                let input = bot_core::SteerInput {
                    session_id: thread_id.clone(),
                    content,
                    sender_user_id: config.sender_user_id,
                    sender_username: config.sender_username,
                    message_id: Some(uuid::Uuid::new_v4().to_string()),
                    chat_type: config.chat_type,
                    platform: Some(config.id),
                    app_id: Some(app_id.to_string()),
                };
                let _ = reply.send(Ok(runtime.bot.submit_subagent_steer(&thread_id, input)));
            }
            Command::CancelSubSession { thread_id, reply } => {
                let _ = reply.send(Ok(runtime.bot.cancel_subagent(&thread_id)));
            }
            Command::ResolveSession {
                platform,
                channel_id,
                reply,
            } => {
                let mut sessions = runtime.sessions.lock().await;
                let value = sessions
                    .create_channel(&platform, &channel_id, &runtime.root_agent_id, None)
                    .and_then(|session| {
                        sessions.set_metadata_string(
                            &session.id,
                            SESSION_APP_ID_METADATA_KEY,
                            &app_id,
                        )?;
                        sessions
                            .get(&session.id)
                            .context("resolved session disappeared after metadata update")
                    });
                let _ = reply.send(value);
            }
            Command::ListSessions { reply } => {
                let mut sessions = runtime.sessions.lock().await.list();
                for session in &mut sessions {
                    interpret_legacy_app_id(session, &app_id);
                }
                let _ = reply.send(Ok(sessions));
            }
            Command::GetSession { id, reply } => {
                let mut session = runtime.sessions.lock().await.get(&id);
                if let Some(session) = &mut session {
                    interpret_legacy_app_id(session, &app_id);
                }
                let _ = reply.send(Ok(session));
            }
            Command::RenameSession { id, title, reply } => {
                let value = runtime.sessions.lock().await.rename(&id, &title);
                let _ = reply.send(value);
            }
            Command::ForkSession {
                source_id,
                platform,
                channel_id,
                title,
                user_id,
                reply,
            } => {
                let value = match runtime.sessions.lock().await.fork_session(
                    &source_id,
                    &platform,
                    &channel_id,
                    title,
                ) {
                    Ok(Some(mut fork)) => {
                        let metadata_result = runtime.sessions.lock().await.set_metadata_string(
                            &fork.id,
                            SESSION_APP_ID_METADATA_KEY,
                            &app_id,
                        );
                        if !matches!(metadata_result, Ok(true)) {
                            let cleanup = runtime.sessions.lock().await.delete(&fork.id);
                            let error = match metadata_result {
                                Ok(false) => {
                                    anyhow!("forked session disappeared before metadata update")
                                }
                                Err(error) => error,
                                Ok(true) => unreachable!(),
                            };
                            let value = match cleanup {
                                Ok(_) => Err(error.context("persisting fork app id (new session rolled back)")),
                                Err(cleanup_error) => Err(error.context(format!("persisting fork app id; rollback also failed: {cleanup_error:#}"))),
                            };
                            let _ = reply.send(value);
                            continue;
                        }
                        fork.metadata.insert(
                            SESSION_APP_ID_METADATA_KEY.into(),
                            serde_json::Value::String(app_id.to_string()),
                        );
                        match runtime
                            .bot
                            .fork_thread_data(&source_id, &fork.id, user_id.as_deref())
                            .await
                        {
                            Ok(()) => Ok(Some(fork)),
                            Err(error) => {
                                let cleanup = runtime.sessions.lock().await.delete(&fork.id);
                                match cleanup {
                                    Ok(_) => Err(anyhow!(error).context("copying forked session state (new session rolled back)")),
                                    Err(cleanup_error) => Err(anyhow!(error).context(format!("copying forked session state; rollback also failed: {cleanup_error:#}"))),
                                }
                            }
                        }
                    }
                    other => other,
                };
                let _ = reply.send(value);
            }
            Command::DeleteSession { id, reply } => {
                let value = runtime.sessions.lock().await.delete(&id);
                let _ = reply.send(value);
            }
            Command::History { id, reply } => {
                let bot = runtime.bot.clone();
                tokio::task::spawn_local(async move {
                    let _ = reply.send(Ok(bot.thread_history(&id).await));
                });
            }
            Command::Submit {
                request,
                config,
                next,
                reply,
            } => {
                tracing::debug!(app_id = %app_id, session_id = %request.session_id, next, "application.submit");
                let req = chat_request(request, config, None, &app_id);
                let value = if next {
                    runtime.submit_next_turn(req)
                } else {
                    runtime.submit_steer(req)
                };
                let _ = reply.send(Ok(value));
            }
            Command::Run {
                request,
                config,
                reply,
            } => {
                let id = next_id;
                next_id += 1;
                let session_id = request.session_id.clone();
                let metadata_updated = runtime.sessions.lock().await.set_metadata_string(
                    &session_id,
                    SESSION_APP_ID_METADATA_KEY,
                    &app_id,
                );
                match metadata_updated {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = reply.send(Err(anyhow!("session not found: {session_id}")));
                        continue;
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                }
                let cancel = CancellationToken::new();
                let stop_delivery = CancellationToken::new();
                let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
                let runtime = runtime.clone();
                let finished = internal_tx.clone();
                let run_cancel = cancel.clone();
                let delivery_cancel = cancel.clone();
                let run_stop_delivery = stop_delivery.clone();
                let run_app_id = app_id.clone();
                let run_session_id = session_id.clone();
                let run_telemetry = telemetry.clone();
                let span = tracing::info_span!("application.run", app_id = %run_app_id, session_id = %run_session_id, run_id = id);
                let join = tokio::task::spawn_local(async move {
                    let mut stream = Box::pin(runtime.chat(chat_request(
                        request,
                        config,
                        Some(run_cancel),
                        &run_app_id,
                    )));
                    while let Some(event) = stream.next().await {
                        if let CoreChatEvent::Bot(bot_core::CatEvent::Error(error)) = &event {
                            if let Some(telemetry) = &run_telemetry {
                                telemetry.capture_agent_error(error, "application.run");
                            }
                        }
                        let event = match event {
                            CoreChatEvent::Prefix(v) => ApplicationEvent::Prefix(v),
                            CoreChatEvent::Reply(v) => ApplicationEvent::Reply(v),
                            CoreChatEvent::SupervisorStarted => ApplicationEvent::SupervisorStarted,
                            CoreChatEvent::Bot(v) => ApplicationEvent::Cat(v),
                            CoreChatEvent::Done => ApplicationEvent::Done,
                        };
                        tokio::select! {
                            result = event_tx.send(event) => {
                                if result.is_err() {
                                    while let Some(drained) = stream.next().await {
                                        if let CoreChatEvent::Bot(bot_core::CatEvent::Error(error)) = drained {
                                            if let Some(telemetry) = &run_telemetry {
                                                telemetry.capture_agent_error(&error, "application.run");
                                            }
                                        }
                                    }
                                    break;
                                }
                            }
                            _ = run_stop_delivery.cancelled() => {
                                while let Some(drained) = stream.next().await {
                                    if let CoreChatEvent::Bot(bot_core::CatEvent::Error(error)) = drained {
                                        if let Some(telemetry) = &run_telemetry {
                                            telemetry.capture_agent_error(&error, "application.run");
                                        }
                                    }
                                }
                                break;
                            }
                            _ = delivery_cancel.cancelled() => {
                                while let Some(drained) = stream.next().await {
                                    if let CoreChatEvent::Bot(bot_core::CatEvent::Error(error)) = drained {
                                        if let Some(telemetry) = &run_telemetry {
                                            telemetry.capture_agent_error(&error, "application.run");
                                        }
                                    }
                                }
                                break;
                            }
                        }
                    }
                    let _ = finished.send(Command::RunFinished(id));
                }.instrument(span));
                runs.insert(
                    id,
                    ActiveRun {
                        session_id,
                        cancel,
                        stop_delivery,
                        join,
                    },
                );
                let _ = reply.send(Ok(RunHandle {
                    id,
                    app_id: app_id.clone(),
                    rx: event_rx,
                    commands: command_tx.clone(),
                }));
            }
            Command::Cancel {
                run_id,
                force,
                reply,
            } => {
                let found = if force {
                    runs.remove(&run_id)
                        .map(|run| {
                            run.stop_delivery.cancel();
                            run.cancel.cancel();
                            run.join.abort();
                            true
                        })
                        .unwrap_or(false)
                } else {
                    runs.get(&run_id)
                        .map(|run| {
                            run.cancel.cancel();
                            true
                        })
                        .unwrap_or(false)
                };
                let _ = reply.send(Ok(found));
            }
            Command::ActiveRuns { reply } => {
                let _ = reply.send(Ok(runs
                    .iter()
                    .map(|(&id, run)| ActiveRunInfo {
                        id,
                        app_id: app_id.to_string(),
                        session_id: run.session_id.clone(),
                        state: if run.cancel.is_cancelled() {
                            ActiveRunState::Cancelling
                        } else {
                            ActiveRunState::Running
                        },
                    })
                    .collect()));
            }
            Command::Hooks { reply } => {
                let _ = reply.send(Ok(runtime.bot.hook_manager().statuses().await));
            }
            Command::SetHookTrusted {
                hash,
                trusted,
                reply,
            } => {
                let value = runtime
                    .bot
                    .hook_manager()
                    .set_trusted(&hash, trusted)
                    .await
                    .map_err(anyhow::Error::from);
                let _ = reply.send(value);
            }
            Command::SetHookEnabled {
                hash,
                enabled,
                reply,
            } => {
                let value = runtime
                    .bot
                    .hook_manager()
                    .set_enabled(&hash, enabled)
                    .await
                    .map_err(anyhow::Error::from);
                let _ = reply.send(value);
            }
            Command::ReloadHooks { reply } => {
                let manager = runtime.bot.hook_manager();
                let value = match manager.reload().await {
                    Ok(()) => Ok(HookReloadReport {
                        hooks: manager.statuses().await,
                    }),
                    Err(error) => Err(anyhow!(error)),
                };
                let _ = reply.send(value);
            }
            Command::RunFinished(id) => {
                runs.remove(&id);
            }
            Command::Shutdown(reply) => {
                for run in runs.values() {
                    run.stop_delivery.cancel();
                    run.cancel.cancel();
                }
                for (_, run) in runs.drain() {
                    let mut join = run.join;
                    if tokio::time::timeout(std::time::Duration::from_secs(5), &mut join)
                        .await
                        .is_err()
                    {
                        join.abort();
                        let _ = join.await;
                    }
                }
                let _ = reply.send(Ok(()));
                break;
            }
        }
    }
}

fn interpret_legacy_app_id(session: &mut Session, app_id: &str) {
    if app_id == "remi-cat" {
        session
            .metadata
            .entry(SESSION_APP_ID_METADATA_KEY)
            .or_insert_with(|| serde_json::Value::String("remi-cat".into()));
    }
}

async fn record_sub_session(
    runtime: &Rc<Runtime>,
    parent_id: &str,
    event: &SubSessionEvent,
    app_id: &str,
    config: &ChannelConfig,
) -> anyhow::Result<String> {
    let thread_id = event.sub_thread_id.0.as_str();
    let kind = if event.agent_name == "acp" {
        SubSessionKind::Acp
    } else {
        SubSessionKind::Agent
    };
    let status = match event.event.as_ref() {
        ProtocolEvent::Done => "done",
        ProtocolEvent::Custom { event_type, .. }
            if matches!(
                event_type.as_str(),
                "sub_session_done" | "sub_session_handed_off"
            ) =>
        {
            "done"
        }
        ProtocolEvent::Error { .. } | ProtocolEvent::Cancelled => "error",
        _ => "running",
    };
    let persists_session_state = match event.event.as_ref() {
        ProtocolEvent::RunStart { .. }
        | ProtocolEvent::Done
        | ProtocolEvent::Error { .. }
        | ProtocolEvent::Cancelled => true,
        ProtocolEvent::Custom { event_type, .. } => {
            matches!(
                event_type.as_str(),
                "sub_session_done" | "sub_session_handed_off"
            )
        }
        _ => false,
    };
    let acp_session_id = if persists_session_state && event.agent_name == "acp" {
        runtime.bot.acp_session_id_for_sub_session(thread_id).await
    } else {
        None
    };
    let mut child_metadata = vec![
        (
            "sub_session_parent_session_id".into(),
            serde_json::Value::String(parent_id.into()),
        ),
        (
            "sub_session_thread_id".into(),
            serde_json::Value::String(thread_id.into()),
        ),
        (
            "sub_session_agent".into(),
            serde_json::Value::String(event.agent_name.clone()),
        ),
        (
            SESSION_APP_ID_METADATA_KEY.into(),
            serde_json::Value::String(app_id.to_string()),
        ),
    ];
    if let Some(acp_session_id) = acp_session_id {
        child_metadata.push((
            "sub_session_acp_session_id".into(),
            serde_json::Value::String(acp_session_id),
        ));
    }
    let mut sessions = runtime.sessions.lock().await;
    let child =
        if persists_session_state || sessions.channel_session_id(&config.id, thread_id).is_none() {
            sessions.upsert_sub_session_channel(
                parent_id,
                thread_id,
                kind,
                &event.agent_name,
                event.title.clone(),
                status,
                &config.id,
                &runtime.root_agent_id,
                child_metadata,
            )?
        } else {
            let child_id = sessions
                .channel_session_id(&config.id, thread_id)
                .context("sub-session channel disappeared")?;
            sessions
                .get(&child_id)
                .context("sub-session channel references a missing session")?
        };
    drop(sessions);
    let messages = match event.event.as_ref() {
        ProtocolEvent::RunStart { .. } => {
            vec![bot_core::Message::user(event.title.clone().unwrap_or_else(
                || format!("Sub-session started: {} / {thread_id}", event.agent_name),
            ))]
        }
        ProtocolEvent::Delta { content, .. } if !content.trim().is_empty() => {
            vec![bot_core::Message::assistant(content.clone())]
        }
        ProtocolEvent::Custom { event_type, extra } if event_type == "sub_session_done" => extra
            .get("final_output")
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .map(|v| vec![bot_core::Message::assistant(v.to_string())])
            .unwrap_or_default(),
        ProtocolEvent::Error { message, .. } => {
            vec![bot_core::Message::assistant(format!("Error: {message}"))]
        }
        ProtocolEvent::Cancelled => vec![bot_core::Message::assistant("Error: cancelled")],
        _ => Vec::new(),
    };
    if !messages.is_empty() {
        runtime
            .bot
            .append_thread_messages(&child.id, messages)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let path = runtime
        .data_dir
        .join("tui-subsession-events")
        .join(format!("{}.jsonl", child.id));
    let metadata_path = runtime
        .data_dir
        .join("tui-subsession-events")
        .join(format!("{}.meta.json", child.id));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut bytes = serde_json::to_vec(event)?;
    bytes.push(b'\n');
    use tokio::io::AsyncWriteExt as _;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(&bytes).await?;
    if persists_session_state || !tokio::fs::try_exists(&metadata_path).await? {
        tokio::fs::write(
            metadata_path,
            serde_json::to_vec_pretty(
                &serde_json::json!({"app_id": app_id, "session_id": &child.id}),
            )?,
        )
        .await?;
    }
    Ok(child.id)
}

async fn read_sub_session_events(
    data_dir: &std::path::Path,
    session_id: &str,
    offset: u64,
    limit: usize,
) -> anyhow::Result<SubSessionEventPage> {
    use tokio::io::{AsyncBufReadExt as _, AsyncSeekExt as _};
    let path = data_dir
        .join("tui-subsession-events")
        .join(format!("{session_id}.jsonl"));
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SubSessionEventPage {
                events: Vec::new(),
                next_offset: offset,
            })
        }
        Err(error) => return Err(error.into()),
    };
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut events = Vec::new();
    let mut consumed = 0_u64;
    let mut line = String::new();
    while events.len() < limit.max(1) {
        line.clear();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            break;
        }
        consumed += bytes as u64;
        events.push(serde_json::from_str(line.trim_end())?);
    }
    Ok(SubSessionEventPage {
        events,
        next_offset: offset + consumed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn assert_send_sync<T: Send + Sync>() {}
    #[test]
    fn handles_are_send_sync() {
        assert_send_sync::<ApplicationHandle>();
        assert_send_sync::<ChannelHandle>();
        assert_send_sync::<RunControl>();
    }

    #[test]
    fn application_ids_are_validated() {
        for valid in ["remi-cat", "host.app_2", "A"] {
            assert!(validate_app_id(valid).is_ok());
        }
        for invalid in ["", "has space", "应用", &"a".repeat(65)] {
            assert!(validate_app_id(invalid).is_err());
        }
        assert_eq!(ApplicationBuilder::new("data").app_id, "remi-cat");
    }

    #[test]
    fn application_telemetry_is_opt_in() {
        let builder = ApplicationBuilder::new("data");
        assert!(builder.sentry_dsn.is_none());
        let builder = builder.sentry_dsn("https://public@example.com/1");
        assert_eq!(
            builder.sentry_dsn.as_deref(),
            Some("https://public@example.com/1")
        );
    }

    #[tokio::test]
    async fn empty_application_sentry_dsn_is_rejected() {
        let error = ApplicationBuilder::new("data")
            .sentry_dsn("  ")
            .spawn()
            .await
            .err()
            .expect("empty DSN should fail");
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn legacy_sessions_are_interpreted_only_by_remi_cat() {
        let mut session = Session {
            id: "legacy".into(),
            channel_binding: ChannelBinding {
                platform: "tui".into(),
                channel_id: "legacy".into(),
            },
            root_agent_id: "default".into(),
            title: None,
            metadata: serde_json::Map::new(),
            sub_sessions: Vec::new(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        interpret_legacy_app_id(&mut session, "embedded-host");
        assert!(!session.metadata.contains_key(SESSION_APP_ID_METADATA_KEY));
        interpret_legacy_app_id(&mut session, "remi-cat");
        assert_eq!(session.metadata[SESSION_APP_ID_METADATA_KEY], "remi-cat");
    }

    #[test]
    fn tui_does_not_reference_internal_runtime_types() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for relative in ["src/tui_app.rs", "src/channel/tui.rs"] {
            let source = std::fs::read_to_string(root.join(relative)).unwrap();
            assert!(
                !source.contains("Rc<Runtime>"),
                "{relative} references Rc<Runtime>"
            );
            assert!(
                !source.contains("CancellationToken"),
                "{relative} references an internal cancellation token"
            );
            assert!(
                !source.contains("JoinHandle"),
                "{relative} owns a run JoinHandle"
            );
        }
    }

    #[test]
    fn application_id_is_not_added_to_http_headers() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut sources = String::new();
        for relative in [
            "crates/bot-core/src/runtime/mod.rs",
            "crates/bot-core/src/runtime/model_provider.rs",
            "src/core/chat.rs",
        ] {
            let path = root.join(relative);
            if let Ok(source) = std::fs::read_to_string(path) {
                sources.push_str(&source);
            }
        }
        assert!(!sources.contains("X-Remi-App-Id"));
        assert!(!sources.contains("X-Remi-Application"));
    }

    #[tokio::test]
    async fn duplicate_hook_text_ids_fail_before_runtime_start() {
        let dir = tempfile::tempdir().unwrap();
        let result = ApplicationBuilder::new(dir.path())
            .hook_text("same", bot_core::HookTextFormat::Json, "{}")
            .hook_text("same", bot_core::HookTextFormat::Toml, "")
            .spawn()
            .await;
        let error = match result {
            Ok(_) => panic!("duplicate hook ids unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("unique"));
    }

    #[tokio::test]
    async fn missing_explicit_hook_file_fails_application_start() {
        let dir = tempfile::tempdir().unwrap();
        let result = ApplicationBuilder::new(dir.path())
            .workspace(dir.path())
            .hook_file("missing.json")
            .spawn()
            .await;
        let error = match result {
            Ok(_) => panic!("missing hook file unexpectedly succeeded"),
            Err(error) => error,
        }
        .to_string();
        assert!(
            error.contains("hook") || error.contains("No such file"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn resolved_sessions_return_and_persist_application_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "OPENAI_API_KEY=test-key\n").unwrap();
        let application = ApplicationBuilder::new(dir.path())
            .app_id("embedded-host")
            .workspace(dir.path())
            .credentials(std::collections::BTreeMap::from([(
                "OPENAI_API_KEY".to_string(),
                "test-key".to_string(),
            )]))
            .discover_hooks(false)
            .include_default_skills(false)
            .file_skills(false)
            .spawn()
            .await
            .unwrap();
        let channel = application
            .handle()
            .channel(ChannelConfig::new("host-channel"));
        let session = channel.resolve_session("conversation").await.unwrap();
        assert_eq!(
            session.metadata[SESSION_APP_ID_METADATA_KEY],
            "embedded-host"
        );
        let persisted = channel.session(&session.id).await.unwrap().unwrap();
        assert_eq!(
            persisted.metadata[SESSION_APP_ID_METADATA_KEY],
            "embedded-host"
        );
        let event = SubSessionEvent::new(
            "parent-tool",
            remi_agentloop::prelude::ThreadId("child-thread".into()),
            remi_agentloop::prelude::RunId("child-run".into()),
            "coder",
            Some("child".into()),
            1,
            ProtocolEvent::RunStart {
                thread_id: "child-thread".into(),
                run_id: "child-run".into(),
                metadata: None,
            },
        );
        let child_id = channel
            .record_sub_session(&session.id, event)
            .await
            .unwrap();
        let child = channel.session(child_id).await.unwrap().unwrap();
        assert_eq!(child.channel_binding.platform, "host-channel");
        assert_eq!(child.metadata[SESSION_APP_ID_METADATA_KEY], "embedded-host");
        let data_dir = application.handle().info().data_dir;
        let sessions_before_delta = std::fs::read(data_dir.join("sessions.json"))
            .expect("read session store before streaming event");
        let delta = SubSessionEvent::new(
            "parent-tool",
            remi_agentloop::prelude::ThreadId("child-thread".into()),
            remi_agentloop::prelude::RunId("child-run".into()),
            "coder",
            Some("child".into()),
            2,
            ProtocolEvent::ThinkingEnd {
                content: "streaming".into(),
            },
        );
        channel
            .record_sub_session(&session.id, delta)
            .await
            .unwrap();
        let sessions_after_delta = std::fs::read(data_dir.join("sessions.json"))
            .expect("read session store after streaming event");
        assert_eq!(sessions_after_delta, sessions_before_delta);
        let handed_off = SubSessionEvent::new(
            "parent-tool",
            remi_agentloop::prelude::ThreadId("child-thread".into()),
            remi_agentloop::prelude::RunId("child-run".into()),
            "coder",
            Some("child".into()),
            3,
            ProtocolEvent::Custom {
                event_type: "sub_session_handed_off".into(),
                extra: serde_json::json!({}),
            },
        );
        channel
            .record_sub_session(&session.id, handed_off)
            .await
            .unwrap();
        let parent = channel.session(&session.id).await.unwrap().unwrap();
        assert_eq!(parent.sub_sessions[0].status, "done");
        application.shutdown().await.unwrap();
    }

    #[test]
    fn embedding_sources_are_independent() {
        let builder = ApplicationBuilder::new("/tmp/ferret-runtime")
            .model_source("/tmp/remi-models")
            .include_default_skills(false)
            .file_skills(false);
        assert_eq!(builder.data_dir, PathBuf::from("/tmp/ferret-runtime"));
        assert_eq!(
            builder.model_source,
            Some(PathBuf::from("/tmp/remi-models"))
        );
        assert!(!builder.include_default_skills);
        assert!(!builder.file_skills);
    }

    #[test]
    fn application_credentials_are_not_exported_to_process_environment() {
        let source = include_str!("application.rs");
        let forbidden = ["apply_entries", "_to_env"].concat();
        assert!(!source.contains(&forbidden));
        assert!(source.contains("from_model_source_with_api_keys"));
    }
}
