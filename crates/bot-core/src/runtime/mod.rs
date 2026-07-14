use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use async_stream::stream;
use bot_runtime_core::ToolContext;
use bot_runtime_core::{CoreSteerInput, CoreSteerQueue, CoreSteerSource, CoreStreamOptions};
use futures::{Stream, StreamExt};
use remi_agentloop::prelude::{
    AgentBuilder, AgentConfig, AgentError, CancellationToken, LoopInput, MessageId, OpenAIClient,
    ProtocolEvent, ResumePayload, Role, RunId, SubSessionEvent, ThreadId, Tool, ToolOutput,
    ToolResult,
};
use remi_agentloop::tool::registry::DefaultToolRegistry;
use tokio::sync::Mutex as AsyncMutex;

use crate::approval::ModelApprovalReviewer;
use crate::hooks::HookContext;
use crate::memory::{build_injected_history, LlmCompressor};
use crate::sandbox::SandboxConfig;
use crate::skill::store::SkillStore;
use crate::tools::{BashMode, SecretRedactor};
use crate::{
    acp, api_key_from_env, embedded_agent_profile, goal, install_embedded_agent_profiles,
    install_embedded_model_profiles, model_usage, remi_skill, resolve_model_profile_from_env,
    sandbox, skill, supervisor_workflow, todo, AccountUsage, AgentModelBindings, AgentProfile,
    AgentRegistry, BuiltinSkillStore, CatAgent, CatEvent, Content, ContentPart, FileSkillStore,
    GoalMaxRounds, GoalState, HookEventName, HookManager, ImAttachment, ImDocument, ImFileBridge,
    MemoryStore, Message, ModelProfileConfig, ModelProfileRegistry, ReasoningEffort,
    SharedRedactor, SkillDocument, SkillLoadDiagnostic, SkillSummary, SteerQueuedEvent,
    SupervisorTraceEvent, ThreadHistoryMessage, ToolApprovalManager, UserQuestionManager,
    WorkflowDefinition, WorkflowInstance, WorkflowMaxRounds, WorkflowReport, WorkflowStatus,
};

mod approval_markers;
mod environment_context;
mod memory_runtime;
mod model_provider;
mod partial_turn;
mod prompting;
mod supervisor_agent;
mod tool_registry;
mod tool_status;
pub(crate) use approval_markers::{
    cat_event_from_subagent_approval_marker, subagent_approval_marker,
    tool_approval_requested_marker, tool_approval_resolved_marker, tool_approval_updated_marker,
    user_question_requested_marker, user_question_resolved_marker, user_question_updated_marker,
};
use environment_context::{
    ensure_environment_context, insert_environment_context_prompt, persist_new_environment_context,
    remove_environment_context, EnvironmentContextSource,
};
use memory_runtime::{persist_intermediate_user_state, persist_turn};
use model_provider::{
    auto_compress_context_percent, build_inner_agent, context_percent_tokens,
    resolve_effective_model_profile, tool_output_overflow_bytes_from_env, InnerAgent,
};
pub use model_provider::{EffectiveModelProfile, EffectiveModelSource};
pub(crate) use partial_turn::PartialTurnRecorder;
pub(crate) use prompting::{
    append_thread_todo_system_prompt, apply_skill_injections, insert_pinned_skill_prompt,
    insert_single_chat_sender_system_prompt, insert_skill_injection_prompts, is_direct_chat,
    model_input_snapshot_from_loop_input, prepend_group_sender_username, route_thread_todo_prompt,
    single_chat_sender_system_prompt, truncate_user_name, SkillPromptToolAvailability,
};
use supervisor_agent::{
    hook_context_message, workflow_round_allows_continue, SupervisorNodeOutcome,
    WorkflowRoundOutcome,
};
use tool_registry::{build_subagent_tools, register_runtime_tools};
pub(crate) use tool_status::{
    builtin_tool_catalog, tool_errors, tool_runtime_errors, tool_warnings,
};

const DEFAULT_AGENT_ID: &str = "default";
const SUPERVISOR_TIMEOUT: Duration = Duration::from_secs(180);
const SUPERVISOR_DECISION_MAX_ATTEMPTS: usize = 3;
const DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT: usize = 80;
const SUPERVISOR_PROMPT_CONTEXT_PERCENT: usize = 80;
const SUPERVISOR_PROMPT_MARGIN_TOKENS: u32 = 4_096;
const MODEL_PROTOCOL_MARGIN_TOKENS: u32 = 512;
const AGENT_MD_CWD_SYSTEM_PROMPT_NOTICE: &str =
    "Agent.md exists in the current working directory; read it before substantive work and follow any applicable project instructions.";

fn supervisor_prompt_budget_tokens(profile: &ModelProfileConfig) -> u32 {
    let percent_budget = (profile.context_tokens as usize)
        .saturating_mul(SUPERVISOR_PROMPT_CONTEXT_PERCENT)
        .div_ceil(100) as u32;
    let output_budget = profile
        .context_tokens
        .saturating_sub(profile.max_output_tokens)
        .saturating_sub(SUPERVISOR_PROMPT_MARGIN_TOKENS);
    percent_budget.min(output_budget).max(1)
}

fn memory_compaction_budget_tokens(
    profile: &ModelProfileConfig,
    context_percent: usize,
    fixed_system_prompt: &str,
) -> usize {
    let percent_budget = context_percent_tokens(profile, context_percent) as u32;
    let safety_margin = profile.context_tokens.saturating_mul(5) / 100;
    let output_safe_budget = profile
        .context_tokens
        .saturating_sub(profile.max_output_tokens)
        .saturating_sub(safety_margin);
    percent_budget
        .min(output_safe_budget)
        .saturating_sub(crate::estimate_model_input_tokens(fixed_system_prompt))
        .saturating_sub(MODEL_PROTOCOL_MARGIN_TOKENS)
        .max(1) as usize
}

fn model_request_budget_tokens(profile: &ModelProfileConfig, context_percent: usize) -> u32 {
    let percent_budget = context_percent_tokens(profile, context_percent) as u32;
    let safety_margin = profile.context_tokens.saturating_mul(5).div_ceil(100);
    percent_budget
        .min(
            profile
                .context_tokens
                .saturating_sub(profile.max_output_tokens)
                .saturating_sub(safety_margin),
        )
        .max(1)
}

fn system_prompt_with_agent_md_notice_for_current_dir(system_prompt: String) -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return system_prompt;
    };
    system_prompt_with_agent_md_notice(system_prompt, &cwd)
}

fn system_prompt_with_agent_md_notice(mut system_prompt: String, cwd: &std::path::Path) -> String {
    if !cwd.join("Agent.md").is_file() {
        return system_prompt;
    }
    if system_prompt.contains(AGENT_MD_CWD_SYSTEM_PROMPT_NOTICE) {
        return system_prompt;
    }
    if !system_prompt.ends_with('\n') {
        system_prompt.push('\n');
    }
    system_prompt.push_str(AGENT_MD_CWD_SYSTEM_PROMPT_NOTICE);
    system_prompt
}

fn supervisor_retry_feedback(error: &str, raw_output: &str) -> String {
    let mut raw = raw_output.trim().chars().take(4_000).collect::<String>();
    if raw_output.trim().chars().count() > 4_000 {
        raw.push_str("\n...[truncated]");
    }
    format!(
        "Your previous supervisor decision was invalid and was rejected before applying to the workflow.\n\
         Error: {error}\n\n\
         Previous output:\n{raw}\n\n\
         Regenerate the decision now. Return only one JSON object. Use only allowed edge ids or allowed target_node ids from the workflow payload. Use JSON null for absent optional fields, not the string \"null\"."
    )
}

pub(crate) fn metadata_flag_enabled(metadata: Option<&serde_json::Value>, key: &str) -> bool {
    metadata
        .and_then(|value| value.get(key))
        .map(|value| {
            value.as_bool().unwrap_or_else(|| {
                value
                    .as_str()
                    .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn steer_preview(content: &Content) -> String {
    let text = content.text_content();
    let trimmed = text.trim();
    let mut preview = trimmed.chars().take(120).collect::<String>();
    if trimmed.chars().count() > 120 {
        preview.push('…');
    }
    preview
}

fn steer_message_metadata(input: &SteerInput) -> Option<serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("steer".into(), serde_json::Value::Bool(true));
    if let Some(value) = input.message_id.as_deref() {
        map.insert(
            "message_id".into(),
            serde_json::Value::String(value.to_string()),
        );
    }
    if let Some(value) = input.chat_type.as_deref() {
        map.insert(
            "chat_type".into(),
            serde_json::Value::String(value.to_string()),
        );
    }
    if let Some(value) = input.platform.as_deref() {
        map.insert(
            "platform".into(),
            serde_json::Value::String(value.to_string()),
        );
    }
    if let Some(value) = input.sender_user_id.as_deref() {
        map.insert(
            "sender_user_id".into(),
            serde_json::Value::String(value.to_string()),
        );
    }
    Some(serde_json::Value::Object(map))
}

fn user_message_metadata(opts: &StreamOptions) -> Option<serde_json::Value> {
    let mut map = serde_json::Map::new();
    if let Some(value) = opts.sender_user_id.as_deref() {
        map.insert(
            "sender_user_id".into(),
            serde_json::Value::String(value.to_string()),
        );
    }
    if let Some(value) = opts
        .sender_username
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        map.insert(
            "sender_username".into(),
            serde_json::Value::String(value.to_string()),
        );
    }
    if let Some(value) = opts.message_id.as_deref() {
        map.insert(
            "message_id".into(),
            serde_json::Value::String(value.to_string()),
        );
    }
    if let Some(value) = opts.chat_type.as_deref() {
        map.insert(
            "chat_type".into(),
            serde_json::Value::String(value.to_string()),
        );
    }
    if let Some(value) = opts.platform.as_deref() {
        map.insert(
            "platform".into(),
            serde_json::Value::String(value.to_string()),
        );
    }
    if !opts.im_attachments.is_empty() {
        map.insert(
            "im_attachments".into(),
            serde_json::Value::String(
                serde_json::to_string(&opts.im_attachments).unwrap_or_default(),
            ),
        );
    }
    if !opts.im_documents.is_empty() {
        map.insert(
            "im_documents".into(),
            serde_json::Value::String(
                serde_json::to_string(&opts.im_documents).unwrap_or_default(),
            ),
        );
    }
    (!map.is_empty()).then_some(serde_json::Value::Object(map))
}

fn background_task_completion_content(task: &crate::ToolTaskRecord) -> Content {
    let args_preview = background_task_args_preview(&task.args);
    let mut text = format!(
        "Background tool task completed.\n\
         tool: {}\n\
         task_id: {}\n\
         status: {}\n\
         args: {}\n\
         elapsed_ms: {}\n",
        task.tool_name,
        task.task_id,
        task.status,
        args_preview,
        task.elapsed_ms.unwrap_or(0)
    );
    if let Some(success) = task.success {
        text.push_str(&format!("success: {success}\n"));
    }
    if let Some(message) = task.message.as_deref() {
        text.push_str(&format!("message: {message}\n"));
    }
    if !task.recent_output.is_empty() {
        text.push_str("recent_output:\n");
        text.push_str(&task.recent_output.join("\n"));
        text.push('\n');
    }
    if let Some(result) = task.result_preview.as_deref() {
        text.push_str("result:\n");
        text.push_str(result);
    }
    Content::text(text)
}

fn background_task_args_preview(args: &serde_json::Value) -> String {
    let rendered = serde_json::to_string(args).unwrap_or_else(|_| args.to_string());
    rendered.chars().take(50).collect()
}

fn background_task_completion_steer_input(task: &crate::ToolTaskRecord) -> CoreSteerInput {
    let args_preview = background_task_args_preview(&task.args);
    CoreSteerInput {
        id: format!("tool-task-{}", task.task_id),
        content: background_task_completion_content(task),
        preview: format!(
            "Background task {} ({}) completed",
            task.task_id, task.tool_name
        ),
        message_metadata: Some(serde_json::json!({
            "background_tool_task": true,
            "task_id": task.task_id,
            "tool_name": task.tool_name,
            "status": task.status,
            "args": args_preview,
        })),
        user_name: None,
        source: CoreSteerSource::BackgroundToolCompletion,
    }
}

fn try_recv_completed_tool_task(
    completed_tool_tasks: &mut tokio::sync::broadcast::Receiver<crate::ToolTaskRecord>,
    thread_id: &str,
) -> Option<crate::ToolTaskRecord> {
    loop {
        match completed_tool_tasks.try_recv() {
            Ok(task) if task.thread_id == thread_id => return Some(task),
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return None,
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return None,
        }
    }
}

fn try_recv_background_side_event(
    background_side_events: &mut tokio::sync::broadcast::Receiver<(String, CatEvent)>,
    thread_id: &str,
) -> Option<CatEvent> {
    loop {
        match background_side_events.try_recv() {
            Ok((event_thread_id, event)) if event_thread_id == thread_id => return Some(event),
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return None,
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return None,
        }
    }
}

pub const ASYNC_TOOL_SYSTEM_PROMPT: &str = "\
Async tool execution:
- Tool calls can keep running in the background after the foreground observation window closes.
- You are free to process other user request, any other tasks or patiently waiting by doing nothing.
- Do not continuously poll background tasks. The system will automatically send a system message with the tool name, task_id, elapsed time, and result when the task finishes.
- Use `tool_tasks` only when you need to inspect current status/recent output or cancel an existing background task.";

fn insert_async_tool_system_prompt(history: &mut Vec<Message>, insertion_index: usize) {
    history.insert(
        insertion_index.min(history.len()),
        Message::system(ASYNC_TOOL_SYSTEM_PROMPT),
    );
}

fn async_agent_enabled_from_env() -> bool {
    std::env::var("REMI_ASYNC_AGENT")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

// -- StreamOptions ----------------------------------------------------------

/// Per-turn options for [`CatBot::stream_with_options`].
#[derive(Debug, Default, Clone)]
pub struct StreamOptions {
    /// Optional session-persisted model profile override.
    pub model_profile_id: Option<String>,
    /// Optional session-persisted reasoning effort override for the current model.
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Optional session-persisted root agent override.
    pub agent_id: Option<String>,
    /// Skill documents explicitly loaded by slash-command preprocessing for this turn.
    pub skill_injections: Vec<SkillDocument>,
    /// UUID of the sender (stored in metadata; injected as a
    /// system annotation in group chats so the LLM can distinguish speakers).
    pub sender_user_id: Option<String>,
    /// IM username used for `Message.name` on the current user turn.
    pub sender_username: Option<String>,
    /// Feishu `message_id` of the incoming message (stored in metadata).
    pub message_id: Option<String>,
    /// Feishu `chat_type` — `"group"` or `"p2p"` (stored in metadata;
    /// triggers speaker annotation when `"group"`).
    pub chat_type: Option<String>,
    /// Current IM platform identifier (for example `feishu`).
    pub platform: Option<String>,
    /// Marks the request as an internal supervisor run.
    /// Goal supervision is disabled for these runs to avoid recursive loops.
    pub supervisor_run: bool,
    /// Marks a user-role message as an internal supervisor instruction. It
    /// remains a user message in model context, but UIs can render it as a
    /// supervisor event when rebuilding persisted history.
    pub internal_supervisor_message: bool,
    /// Downloadable IM attachments referenced by the current message.
    pub im_attachments: Vec<ImAttachment>,
    /// Feishu document links referenced by the current message.
    pub im_documents: Vec<ImDocument>,
    /// Optional cooperative-cancel signal. When cancelled, `stream_with_options`
    /// will persist any
    /// already-generated content and yield a final [`CatEvent::Done`] before
    /// returning — so memory is not lost on preemption.
    pub cancel: Option<CancellationToken>,
    /// Enables automatic background task completion steer and keeps the turn
    /// running while background tool tasks are active.
    pub async_agent: bool,
}

// -- Type aliases -------------------------------------------------------------

type ThreadRunLock = Arc<AsyncMutex<()>>;
type ThreadRunLocks = Arc<AsyncMutex<HashMap<String, ThreadRunLock>>>;
type ActiveSteerQueues = Arc<StdMutex<HashMap<String, Arc<CoreSteerQueue>>>>;

#[derive(Debug, Clone)]
pub struct SteerInput {
    pub session_id: String,
    pub content: Content,
    pub sender_user_id: Option<String>,
    pub sender_username: Option<String>,
    pub message_id: Option<String>,
    pub chat_type: Option<String>,
    pub platform: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SteerSubmitResult {
    Queued(SteerQueuedEvent),
    NotRunning,
}

struct LocalAcpAgentRunner {
    agent: CatAgent<InnerAgent>,
    memory: Arc<MemoryStore>,
    run_locks: ThreadRunLocks,
    system_prompt: String,
    environment_context_source: EnvironmentContextSource,
    model_profile: ModelProfileConfig,
}

struct SteerQueueRegistration {
    session_id: String,
    queue: Arc<CoreSteerQueue>,
    active: ActiveSteerQueues,
    removes_queue_on_drop: bool,
}

impl Drop for SteerQueueRegistration {
    fn drop(&mut self) {
        if !self.removes_queue_on_drop {
            return;
        }
        let mut active = self
            .active
            .lock()
            .expect("active steer queue lock poisoned");
        if active
            .get(&self.session_id)
            .is_some_and(|queue| Arc::ptr_eq(queue, &self.queue))
        {
            active.remove(&self.session_id);
        }
    }
}

fn local_acp_thread_id(session_id: &str) -> String {
    format!("acp:{session_id}")
}

async fn thread_run_lock(locks: &ThreadRunLocks, thread_id: &str) -> ThreadRunLock {
    let mut locks = locks.lock().await;
    locks
        .entry(thread_id.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

impl acp::AcpLocalRunner for LocalAcpAgentRunner {
    fn run<'a>(
        &'a self,
        session_id: &'a str,
        message: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + 'a>> {
        Box::pin(async move {
            let thread_id = local_acp_thread_id(session_id);
            let run_lock = thread_run_lock(&self.run_locks, &thread_id).await;
            let _run_guard = run_lock.lock().await;
            let mut ctx = self.memory.load_context(&thread_id).await?;
            let environment_context =
                ensure_environment_context(&mut ctx.user_state, &self.environment_context_source);
            persist_new_environment_context(
                &self.memory,
                &thread_id,
                &ctx.user_state,
                environment_context.initialized,
            )
            .await;
            let mut history = build_injected_history(&ctx);
            history.insert(0, Message::system(self.system_prompt.clone()));
            insert_environment_context_prompt(&mut history, 1, environment_context.prompt.clone());
            append_thread_todo_system_prompt(&mut history, &ctx.user_state);
            let mut skip_count = history.len();
            let user_state = ctx.user_state.clone();
            let mut input = LoopInput::start(message)
                .history(history)
                .metadata(serde_json::json!({ "thread_id": &thread_id }))
                .user_state(user_state.clone());
            let budget = model_request_budget_tokens(
                &self.model_profile,
                auto_compress_context_percent().unwrap_or(DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT),
            );
            loop {
                let definitions = self.agent.tool_definitions_for_input(&input, None);
                let estimated = model_input_snapshot_from_loop_input(
                    &input,
                    &definitions,
                    &thread_id,
                    None,
                    &self.model_profile.id,
                    &self.model_profile.model,
                )
                .map(|snapshot| snapshot.totals.estimated_tokens)
                .unwrap_or(0);
                if estimated <= budget {
                    break;
                }
                self.memory.compact_for_request(&thread_id).await.map_err(|err| {
                    anyhow::anyhow!(
                        "ACP request exceeds context budget ({estimated} > {budget}) and compression failed: {err}"
                    )
                })?;
                let mut refreshed = self.memory.load_context(&thread_id).await?;
                refreshed.user_state = user_state.clone();
                let mut rebuilt = build_injected_history(&refreshed);
                rebuilt.insert(0, Message::system(self.system_prompt.clone()));
                insert_environment_context_prompt(
                    &mut rebuilt,
                    1,
                    environment_context.prompt.clone(),
                );
                append_thread_todo_system_prompt(&mut rebuilt, &refreshed.user_state);
                skip_count = rebuilt.len();
                input = LoopInput::start(message)
                    .history(rebuilt)
                    .metadata(serde_json::json!({ "thread_id": &thread_id }))
                    .user_state(user_state.clone());
            }
            let mut stream = std::pin::pin!(self.agent.stream_with_input(input));
            let mut text = String::new();
            let mut raw_history: Option<Vec<Message>> = None;
            let mut raw_user_state: Option<serde_json::Value> = None;
            while let Some(event) = stream.next().await {
                match event {
                    CatEvent::Text(delta) => text.push_str(&delta),
                    CatEvent::History(messages, user_state) => {
                        raw_history = Some(messages);
                        raw_user_state = Some(user_state);
                    }
                    CatEvent::StateUpdate(user_state) => {
                        raw_user_state = Some(user_state);
                    }
                    CatEvent::Done => break,
                    CatEvent::Error(err) => {
                        anyhow::bail!("local ACP agent failed for session {session_id}: {err}")
                    }
                    _ => {}
                }
            }
            persist_turn(
                &self.memory,
                &thread_id,
                raw_history,
                raw_user_state,
                skip_count,
                &HashMap::new(),
            )
            .await;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                anyhow::bail!("local ACP agent produced no text for session {session_id}");
            }
            Ok(trimmed.to_string())
        })
    }
}

// -- CatBot -------------------------------------------------------------------

/// Main bot handle.  Build with [`CatBotBuilder`] or [`CatBot::from_env`].
pub struct CatBot {
    inner: CatAgent<InnerAgent>,
    model_agents: HashMap<String, CatAgent<InnerAgent>>,
    agent_runtimes: HashMap<String, HashMap<String, CatAgent<InnerAgent>>>,
    agent_profiles: HashMap<String, AgentProfile>,
    default_agent_profile: AgentProfile,
    skill_store: Arc<BuiltinSkillStore<FileSkillStore>>,
    pinned_skill_summaries: Vec<SkillSummary>,
    memory: Arc<MemoryStore>,
    todo_backend: Arc<todo::HybridTodoBackend>,
    acp_backend: Arc<acp::AcpBackend>,
    approval_manager: Arc<ToolApprovalManager>,
    user_question_manager: Arc<UserQuestionManager>,
    hook_manager: Arc<HookManager>,
    tool_tasks: Arc<crate::tool_tasks::ToolTaskManager>,
    run_locks: ThreadRunLocks,
    active_steers: ActiveSteerQueues,
    model_profile: ModelProfileConfig,
    model_registry: Arc<ModelProfileRegistry>,
    environment_context_source: EnvironmentContextSource,
    /// Shared secret redactor — updated via `update_secret_redactor`.
    redactor: SharedRedactor,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegisteredToolStatus {
    pub name: String,
    pub description: String,
    pub registered: bool,
    pub enabled: bool,
    pub in_active_allowlist: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EffectiveAgentProfile {
    pub profile: AgentProfile,
    pub source: EffectiveAgentSource,
    pub invalid_session_agent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveAgentSource {
    Workflow,
    Session,
    Default,
}

impl CatBot {
    fn register_steer_queue(
        &self,
        session_id: &str,
    ) -> (Arc<CoreSteerQueue>, SteerQueueRegistration) {
        let mut active = self
            .active_steers
            .lock()
            .expect("active steer queue lock poisoned");
        let (queue, removes_queue_on_drop) = match active.get(session_id) {
            Some(queue) => (Arc::clone(queue), false),
            None => {
                let queue = Arc::new(CoreSteerQueue::new());
                active.insert(session_id.to_string(), Arc::clone(&queue));
                (queue, true)
            }
        };
        let guard = SteerQueueRegistration {
            session_id: session_id.to_string(),
            queue: Arc::clone(&queue),
            active: Arc::clone(&self.active_steers),
            removes_queue_on_drop,
        };
        (queue, guard)
    }

    pub fn submit_steer(&self, input: SteerInput) -> SteerSubmitResult {
        self.submit_queued_input(input, CoreSteerSource::User)
    }

    pub fn submit_next_turn(&self, input: SteerInput) -> SteerSubmitResult {
        self.submit_queued_input(input, CoreSteerSource::UserNextTurn)
    }

    fn submit_queued_input(&self, input: SteerInput, source: CoreSteerSource) -> SteerSubmitResult {
        let queue = {
            self.active_steers
                .lock()
                .expect("active steer queue lock poisoned")
                .get(&input.session_id)
                .cloned()
        };
        let Some(queue) = queue else {
            return SteerSubmitResult::NotRunning;
        };
        let steer_id = uuid::Uuid::new_v4().to_string();
        let preview = steer_preview(&input.content);
        let message_metadata = steer_message_metadata(&input);
        queue.push(CoreSteerInput {
            id: steer_id.clone(),
            content: input.content,
            preview: preview.clone(),
            message_metadata,
            user_name: input.sender_username.clone(),
            source,
        });
        SteerSubmitResult::Queued(SteerQueuedEvent {
            steer_id,
            session_id: input.session_id,
            preview,
        })
    }

    pub fn model_context_tokens(&self) -> u32 {
        self.model_profile.context_tokens
    }

    pub fn approval_manager(&self) -> Arc<ToolApprovalManager> {
        Arc::clone(&self.approval_manager)
    }

    pub fn user_question_manager(&self) -> Arc<UserQuestionManager> {
        Arc::clone(&self.user_question_manager)
    }

    pub async fn answer_user_question(
        &self,
        question_id: &str,
        answer: crate::user_question::UserQuestionResponse,
    ) -> Option<crate::user_question::UserQuestionRequest> {
        let request = self
            .user_question_manager
            .answer(question_id, answer)
            .await?;
        let _ = self
            .resume_workflow_from_user_input(&request.session_id, 0)
            .await;
        Some(request)
    }

    pub fn hook_manager(&self) -> Arc<HookManager> {
        Arc::clone(&self.hook_manager)
    }

    pub fn model_context_tokens_for(&self, session_model_profile_id: Option<&str>) -> u32 {
        self.effective_model_profile(session_model_profile_id)
            .profile
            .context_tokens
    }

    pub fn model_context_tokens_for_agent(
        &self,
        session_model_profile_id: Option<&str>,
        session_agent_id: Option<&str>,
    ) -> u32 {
        let effective_agent = self.effective_agent_profile(session_agent_id);
        self.effective_model_profile_for_agent(session_model_profile_id, &effective_agent.profile)
            .profile
            .context_tokens
    }

    pub fn model_profiles(&self) -> Vec<&ModelProfileConfig> {
        self.model_registry.list()
    }

    pub fn agent_profiles(&self) -> Vec<&AgentProfile> {
        self.agent_profiles.values().collect()
    }

    pub fn default_agent_profile(&self) -> &AgentProfile {
        &self.default_agent_profile
    }

    pub fn get_agent_profile(&self, id: &str) -> Option<&AgentProfile> {
        self.agent_profiles.get(id)
    }

    pub fn skill_summaries(&self) -> Vec<SkillSummary> {
        self.skill_store.featured_summaries()
    }

    pub fn all_skill_summaries(&self) -> Vec<SkillSummary> {
        self.skill_store.all_summaries()
    }

    pub fn skill_load_diagnostics(&self) -> Vec<SkillLoadDiagnostic> {
        self.skill_store.load_diagnostics()
    }

    pub async fn get_skill(&self, name: &str) -> Result<Option<SkillDocument>, AgentError> {
        self.skill_store.get(name).await
    }

    pub async fn read_skill_names(&self, thread_id: &str) -> Vec<String> {
        let user_state = self.memory.load_user_state(thread_id).await;
        skill::tools::read_skill_names(&user_state)
    }

    pub fn workflow_definitions(&self) -> Result<Vec<WorkflowDefinition>, AgentError> {
        supervisor_workflow::list_definitions(&self.memory.data_dir).map_err(AgentError::other)
    }

    pub fn default_model_profile(&self) -> &ModelProfileConfig {
        &self.model_profile
    }

    pub fn get_model_profile(&self, id: &str) -> Option<&ModelProfileConfig> {
        self.model_registry.get(id)
    }

    pub fn effective_model_profile(
        &self,
        session_model_profile_id: Option<&str>,
    ) -> EffectiveModelProfile {
        resolve_effective_model_profile(
            &self.model_profile,
            &self.model_registry,
            session_model_profile_id,
        )
    }

    pub fn effective_model_profile_with_reasoning(
        &self,
        session_model_profile_id: Option<&str>,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> anyhow::Result<EffectiveModelProfile> {
        apply_reasoning_effort_override(
            self.effective_model_profile(session_model_profile_id),
            reasoning_effort,
        )
    }

    pub fn effective_agent_profile(&self, session_agent_id: Option<&str>) -> EffectiveAgentProfile {
        self.effective_agent_profile_for_workflow(session_agent_id, None)
    }

    pub fn effective_agent_profile_for_workflow(
        &self,
        session_agent_id: Option<&str>,
        workflow_agent_id: Option<&str>,
    ) -> EffectiveAgentProfile {
        if let Some(id) = workflow_agent_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Some(profile) = self.agent_profiles.get(id) {
                return EffectiveAgentProfile {
                    profile: profile.clone(),
                    source: EffectiveAgentSource::Workflow,
                    invalid_session_agent: None,
                };
            }
            return EffectiveAgentProfile {
                profile: self.default_agent_profile.clone(),
                source: EffectiveAgentSource::Default,
                invalid_session_agent: Some(id.to_string()),
            };
        }
        if let Some(id) = session_agent_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Some(profile) = self.agent_profiles.get(id) {
                return EffectiveAgentProfile {
                    profile: profile.clone(),
                    source: EffectiveAgentSource::Session,
                    invalid_session_agent: None,
                };
            }
            return EffectiveAgentProfile {
                profile: self.default_agent_profile.clone(),
                source: EffectiveAgentSource::Default,
                invalid_session_agent: Some(id.to_string()),
            };
        }
        EffectiveAgentProfile {
            profile: self.default_agent_profile.clone(),
            source: EffectiveAgentSource::Default,
            invalid_session_agent: None,
        }
    }

    fn workflow_node_agent<'a>(&self, instance: &'a WorkflowInstance) -> Option<&'a str> {
        if instance.status != WorkflowStatus::Active {
            return None;
        }
        instance.definition.node_agent(&instance.current_node)
    }

    fn validate_workflow_agents(&self, definition: &WorkflowDefinition) -> Result<(), AgentError> {
        for node in &definition.nodes {
            let Some(agent) = node
                .agent
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            if self.agent_profiles.contains_key(agent) {
                continue;
            }
            return Err(AgentError::other(format!(
                "workflow node `{}` references unknown agent `{agent}`",
                node.id
            )));
        }
        Ok(())
    }

    pub fn effective_model_profile_for_agent(
        &self,
        session_model_profile_id: Option<&str>,
        agent: &AgentProfile,
    ) -> EffectiveModelProfile {
        if let Some(id) = session_model_profile_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return self.effective_model_profile(Some(id));
        }
        let mut effective = self.effective_model_profile(agent.models.primary.as_deref());
        if matches!(effective.source, EffectiveModelSource::Default) {
            effective.invalid_session_model = None;
        }
        if let Some(model) = agent
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            effective.profile.model = model.to_string();
        }
        if let Some(base_url) = agent
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            effective.profile.base_url = Some(base_url.to_string());
        }
        effective
    }

    pub async fn account_usage(&self) -> anyhow::Result<AccountUsage> {
        let api_key = api_key_from_env(&self.model_profile)?;
        model_usage::query_account_usage(&self.model_profile, &api_key).await
    }

    pub async fn account_usage_for(
        &self,
        session_model_profile_id: Option<&str>,
    ) -> anyhow::Result<AccountUsage> {
        let effective = self.effective_model_profile(session_model_profile_id);
        let api_key = api_key_from_env(&effective.profile)?;
        model_usage::query_account_usage(&effective.profile, &api_key).await
    }

    fn agent_for_model_profile(&self, profile_id: &str) -> &CatAgent<InnerAgent> {
        if profile_id == self.model_profile.id {
            &self.inner
        } else {
            self.model_agents.get(profile_id).unwrap_or(&self.inner)
        }
    }

    fn runtime_for_agent_and_model(
        &self,
        agent_id: &str,
        model_profile_id: &str,
    ) -> &CatAgent<InnerAgent> {
        if agent_id == self.default_agent_profile.id {
            return self.agent_for_model_profile(model_profile_id);
        }
        self.agent_runtimes
            .get(agent_id)
            .and_then(|by_model| by_model.get(model_profile_id))
            .unwrap_or_else(|| self.agent_for_model_profile(model_profile_id))
    }

    fn hook_context(
        &self,
        thread_id: &str,
        model: Option<String>,
        turn_id: Option<String>,
    ) -> HookContext {
        HookContext {
            session_id: thread_id.to_string(),
            transcript_path: None,
            cwd: self.inner.workspace_root.clone(),
            model,
            turn_id,
            permission_mode: None,
        }
    }

    pub async fn thread_todos(&self, thread_id: &str) -> Result<Vec<todo::TodoItem>, AgentError> {
        let context = self.memory.load_context(thread_id).await?;
        Ok(todo::todos_from_user_state(&context.user_state))
    }

    /// Convenience constructor — reads credentials from environment variables.
    ///
    /// | Variable                  | Description                                           |
    /// |---------------------------|-------------------------------------------------------|
    /// | `<PROVIDER>_API_KEY`      | Provider-specific model API key, for example `OPENAI_API_KEY`, `MIMO_API_KEY`, or `DEEPSEEK_API_KEY` |
    /// | `REMI_MODEL_PROFILE`      | Selected model profile id (default: `default`)        |
    /// | `REMI_KIMI_THINKING`      | Legacy thinking override for supported Kimi profiles  |
    /// | `REMI_REASONING_EFFORT`   | Optional reasoning effort override (`auto`, `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`) |
    /// | `REMI_MEMORY_DAYS`        | Days before mid-term → long-term (default: 7)         |
    /// | `REMI_AUTO_COMPRESS_CONTEXT_PERCENT` | Context usage percent that triggers auto compression (default: 80) |
    /// | `LANGSMITH_API_KEY`       | Enable LangSmith tracing (optional)                   |
    /// | `LANGSMITH_PROJECT`       | LangSmith project name (default: `remi-cat`)          |
    pub fn from_env() -> anyhow::Result<Self> {
        CatBotBuilder::from_env()?.build()
    }

    /// Immediately flush all short-term memory into a new mid-term block.
    ///
    /// Returns the number of messages compressed, or `0` if already empty.
    pub async fn compact_memory(
        &self,
        thread_id: &str,
    ) -> Result<usize, remi_agentloop::prelude::AgentError> {
        let run_lock = self.thread_run_lock(thread_id).await;
        let _run_guard = run_lock.try_lock().map_err(|_| {
            remi_agentloop::prelude::AgentError::other(
                "cannot compact memory while this session is running; wait for the current model/tool turn to finish or cancel it first",
            )
        })?;
        let context = self.hook_context(thread_id, Some(self.model_profile.model.clone()), None);
        let pre = self
            .hook_manager
            .run(
                HookEventName::PreCompact,
                Some("manual"),
                &context,
                serde_json::json!({ "trigger": "manual" }),
            )
            .await;
        if pre.blocked || pre.continue_flow == Some(false) {
            return Err(remi_agentloop::prelude::AgentError::other(
                pre.reason
                    .unwrap_or_else(|| "context compaction blocked by hook".to_string()),
            ));
        }
        let result = self.memory.compact_now(thread_id).await;
        let payload = match &result {
            Ok(compacted_messages) => serde_json::json!({
                "trigger": "manual",
                "success": true,
                "compacted_messages": compacted_messages,
            }),
            Err(err) => serde_json::json!({
                "trigger": "manual",
                "success": false,
                "error": err.to_string(),
            }),
        };
        let _ = self
            .hook_manager
            .run(
                HookEventName::PostCompact,
                Some("manual"),
                &context,
                payload,
            )
            .await;
        result
    }

    /// Clear conversation history for one thread while preserving tool state.
    pub async fn clear_memory(
        &self,
        thread_id: &str,
    ) -> Result<(), remi_agentloop::prelude::AgentError> {
        let run_lock = self.thread_run_lock(thread_id).await;
        let _run_guard = run_lock.lock().await;
        self.memory.clear_thread(thread_id).await
    }

    pub async fn thread_history(&self, thread_id: &str) -> Vec<ThreadHistoryMessage> {
        self.memory.thread_history(thread_id).await
    }

    pub async fn append_thread_messages(
        &self,
        thread_id: &str,
        messages: Vec<remi_agentloop::prelude::Message>,
    ) -> Result<(), remi_agentloop::prelude::AgentError> {
        if messages.is_empty() {
            return Ok(());
        }
        let run_lock = self.thread_run_lock(thread_id).await;
        let _run_guard = run_lock.lock().await;
        self.memory.save_turn(thread_id, messages).await
    }

    pub async fn delete_thread_data(
        &self,
        thread_id: &str,
        user_id: Option<&str>,
    ) -> Result<(), remi_agentloop::prelude::AgentError> {
        let run_lock = self.thread_run_lock(thread_id).await;
        let _run_guard = run_lock.lock().await;
        self.todo_backend
            .delete_thread(thread_id, user_id)
            .await
            .map_err(|err| AgentError::other(format!("delete thread todos: {err:#}")))?;
        self.memory.delete_thread(thread_id).await?;
        let _ = supervisor_workflow::clear_instance(&self.memory.data_dir, thread_id).await;
        Ok(())
    }

    pub async fn fork_thread_data(
        &self,
        source_thread_id: &str,
        target_thread_id: &str,
        user_id: Option<&str>,
    ) -> Result<(), remi_agentloop::prelude::AgentError> {
        if source_thread_id == target_thread_id {
            return Err(AgentError::other("cannot fork a thread onto itself"));
        }
        let source_lock = self.thread_run_lock(source_thread_id).await;
        let target_lock = self.thread_run_lock(target_thread_id).await;
        if source_thread_id <= target_thread_id {
            let _source_guard = source_lock.lock().await;
            let _target_guard = target_lock.lock().await;
            self.fork_thread_data_locked(source_thread_id, target_thread_id, user_id)
                .await
        } else {
            let _target_guard = target_lock.lock().await;
            let _source_guard = source_lock.lock().await;
            self.fork_thread_data_locked(source_thread_id, target_thread_id, user_id)
                .await
        }
    }

    async fn fork_thread_data_locked(
        &self,
        source_thread_id: &str,
        target_thread_id: &str,
        user_id: Option<&str>,
    ) -> Result<(), remi_agentloop::prelude::AgentError> {
        self.memory
            .fork_thread(source_thread_id, target_thread_id)
            .await?;
        let mut user_state = self.memory.load_user_state(target_thread_id).await;
        remove_environment_context(&mut user_state);
        self.todo_backend
            .fork_thread_user_state(source_thread_id, target_thread_id, user_id, &mut user_state)
            .await
            .map_err(|err| AgentError::other(format!("fork thread todos: {err:#}")))?;
        self.memory
            .save_user_state(target_thread_id, &user_state)
            .await?;
        Ok(())
    }

    pub async fn set_goal(
        &self,
        thread_id: &str,
        goal_text: &str,
        max_rounds: GoalMaxRounds,
    ) -> Result<GoalState, remi_agentloop::prelude::AgentError> {
        let instance = self
            .start_workflow(
                thread_id,
                supervisor_workflow::embedded_goal_definition(),
                serde_json::json!({"goal": goal_text.trim()}),
                max_rounds,
            )
            .await?;
        goal::from_instance(&instance)
            .ok_or_else(|| AgentError::other("failed to create goal workflow"))
    }

    pub async fn goal_status(&self, thread_id: &str) -> Option<GoalState> {
        let instance = self.workflow_status(thread_id).await?;
        goal::from_instance(&instance)
    }

    pub async fn clear_goal(
        &self,
        thread_id: &str,
    ) -> Result<(), remi_agentloop::prelude::AgentError> {
        self.clear_workflow(thread_id).await
    }

    pub async fn start_workflow(
        &self,
        thread_id: &str,
        definition: WorkflowDefinition,
        context: serde_json::Value,
        max_rounds: WorkflowMaxRounds,
    ) -> Result<WorkflowInstance, AgentError> {
        definition.validate().map_err(AgentError::other)?;
        self.validate_workflow_agents(&definition)?;
        if !context.is_object() {
            return Err(AgentError::other("workflow context must be a JSON object"));
        }
        let instance = WorkflowInstance {
            current_node: definition.initial_node.clone(),
            definition,
            context,
            incoming_edge: None,
            node_message: None,
            status: WorkflowStatus::Active,
            pause_reason: None,
            ask_user_for_help: None,
            max_rounds,
            last_report: None,
            updated_at: chrono::Utc::now(),
        };
        self.save_workflow_instance(thread_id, &instance).await?;
        Ok(instance)
    }

    pub async fn start_workflow_by_id(
        &self,
        thread_id: &str,
        workflow_id: &str,
        context: serde_json::Value,
        max_rounds: WorkflowMaxRounds,
    ) -> Result<WorkflowInstance, AgentError> {
        let definition = supervisor_workflow::load_definition(&self.memory.data_dir, workflow_id)
            .await
            .map_err(AgentError::other)?;
        self.start_workflow(thread_id, definition, context, max_rounds)
            .await
    }

    pub fn stream_workflow_start_with_options<'a>(
        &'a self,
        thread_id: &'a str,
        workflow_id: String,
        context: serde_json::Value,
        max_rounds: WorkflowMaxRounds,
        opts: StreamOptions,
    ) -> impl Stream<Item = CatEvent> + 'a {
        let thread_id_owned = thread_id.to_string();
        stream! {
            // Enrich workflow context with current session metadata so the
            // supervisor has visibility into the session configuration.
            let mut context = context;
            if let Some(obj) = context.as_object_mut() {
                if let Some(ref id) = opts.model_profile_id {
                    obj.insert("session_model_profile_id".into(), serde_json::Value::String(id.clone()));
                }
                if let Some(ref id) = opts.agent_id {
                    obj.insert("session_agent_id".into(), serde_json::Value::String(id.clone()));
                }
                if let Some(effort) = opts.reasoning_effort {
                    obj.insert("session_reasoning_effort".into(), serde_json::Value::String(effort.as_str().to_string()));
                }
                if let Some(ref platform) = opts.platform {
                    obj.insert("session_platform".into(), serde_json::Value::String(platform.clone()));
                }
                if let Some(ref chat_type) = opts.chat_type {
                    obj.insert("session_chat_type".into(), serde_json::Value::String(chat_type.clone()));
                }
                if let Some(ref sender) = opts.sender_user_id {
                    obj.insert("session_sender_user_id".into(), serde_json::Value::String(sender.clone()));
                }
                if let Some(ref username) = opts.sender_username {
                    obj.insert("session_sender_username".into(), serde_json::Value::String(username.clone()));
                }
            }
            let instance = match self
                .start_workflow_by_id(
                    &thread_id_owned,
                    &workflow_id,
                    context,
                    max_rounds,
                )
                .await
            {
                Ok(instance) => instance,
                Err(err) => {
                    yield CatEvent::Error(err);
                    return;
                }
            };

            let ctx = match self.memory.load_context(&thread_id_owned).await {
                Ok(ctx) => ctx,
                Err(err) => {
                    yield CatEvent::Error(AgentError::other(err.to_string()));
                    return;
                }
            };
            let history = build_injected_history(&ctx);
            let todo_prompt = todo::latest_unfinished_batch_system_prompt(&ctx.user_state);
            let supervisor_model_profile_id = opts.model_profile_id.clone();
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
            let evaluation = self.evaluate_workflow_after_round(
                &thread_id_owned,
                &history,
                todo_prompt,
                0,
                supervisor_model_profile_id.as_deref(),
                opts.cancel.clone(),
                progress_tx,
            );
            tokio::pin!(evaluation);
            let outcome = loop {
                tokio::select! {
                    outcome = &mut evaluation => break outcome,
                    progress = progress_rx.recv() => {
                        if let Some(progress) = progress {
                            yield CatEvent::SupervisorProgress(progress);
                        }
                    }
                }
            };
            while let Ok(progress) = progress_rx.try_recv() {
                yield CatEvent::SupervisorProgress(progress);
            }

            match outcome {
                WorkflowRoundOutcome::Cancelled => {
                    yield CatEvent::Cancelled;
                    yield CatEvent::Done;
                    return;
                }
                WorkflowRoundOutcome::NoWorkflow => {
                    yield CatEvent::Supervisor(WorkflowReport {
                        workflow_id: instance.definition.id.clone(),
                        workflow_name: instance.definition.name.clone(),
                        from_node: instance.current_node.clone(),
                        edge: None,
                        to_node: instance.current_node.clone(),
                        path_edges: Vec::new(),
                        path_nodes: Vec::new(),
                        status: instance.status.clone(),
                        reason: "workflow did not produce a supervisor decision".to_string(),
                        agent_message: None,
                        next_node_message: None,
                        supervisor_trace: Vec::new(),
                        round: 0,
                        max_rounds: instance.max_rounds.clone(),
                        error: None,
                    });
                }
                WorkflowRoundOutcome::Report(report) => {
                    yield CatEvent::Supervisor(report);
                }
                WorkflowRoundOutcome::Continue { report, message } => {
                    yield CatEvent::Supervisor(report);
                    let mut main_stream = Box::pin(
                        self.stream_with_options(&thread_id_owned, Content::text(message), opts),
                    );
                    while let Some(event) = main_stream.next().await {
                        yield event;
                    }
                }
            }
        }
    }

    /// Handles a user turn for an already active workflow. The supervisor is
    /// always the first model invoked; its message is then passed to the
    /// selected workflow agent as an internal supervisor run.
    pub fn stream_active_workflow_with_options<'a>(
        &'a self,
        thread_id: &'a str,
        content: Content,
        opts: StreamOptions,
    ) -> impl Stream<Item = CatEvent> + 'a {
        let thread_id_owned = thread_id.to_string();
        stream! {
            // Register before any workflow I/O so a second user input during
            // supervisor evaluation is queued as a steer instead of starting
            // a competing workflow turn. The inner agent run reuses this
            // registration and drains the same queue at its normal gaps.
            let steer_registration = (!opts.supervisor_run).then(|| {
                self.register_steer_queue(&thread_id_owned)
            });
            let steer_queue = steer_registration
                .as_ref()
                .map(|(queue, _)| Arc::clone(queue));
            let Some(instance) = self
                .workflow_status(&thread_id_owned)
                .await
                .filter(|instance| instance.status == WorkflowStatus::Active)
            else {
                // This branch is the normal non-workflow turn. Boxing keeps
                // the substantially larger agent stream out of the workflow
                // dispatch stream's concrete future type.
                let mut stream = Box::pin(self.stream_with_options(&thread_id_owned, content, opts));
                while let Some(event) = stream.next().await {
                    yield event;
                }
                return;
            };

            let user_message = Message {
                id: MessageId::new(),
                role: Role::User,
                content: content.clone(),
                tool_calls: None,
                tool_call_id: None,
                name: opts.sender_username.clone(),
                reasoning_content: None,
                metadata: user_message_metadata(&opts),
            };
            if let Err(error) = self
                .append_thread_messages(&thread_id_owned, vec![user_message])
                .await
            {
                yield CatEvent::Error(error);
                return;
            }

            let ctx = match self.memory.load_context(&thread_id_owned).await {
                Ok(ctx) => ctx,
                Err(error) => {
                    yield CatEvent::Error(error);
                    return;
                }
            };
            let history = build_injected_history(&ctx);
            let todo_prompt = todo::latest_unfinished_batch_system_prompt(&ctx.user_state);
            let round = instance.last_report.as_ref().map(|report| report.round).unwrap_or(0);
            let supervisor_model_profile_id = opts.model_profile_id.clone();
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
            let evaluation = self.evaluate_workflow_after_round(
                &thread_id_owned,
                &history,
                todo_prompt,
                round,
                supervisor_model_profile_id.as_deref(),
                opts.cancel.clone(),
                progress_tx,
            );
            tokio::pin!(evaluation);
            let outcome = loop {
                tokio::select! {
                    outcome = &mut evaluation => break outcome,
                    progress = progress_rx.recv() => {
                        if let Some(progress) = progress {
                            yield CatEvent::SupervisorProgress(progress);
                        }
                    }
                }
            };
            while let Ok(progress) = progress_rx.try_recv() {
                yield CatEvent::SupervisorProgress(progress);
            }

            match outcome {
                WorkflowRoundOutcome::Cancelled => {
                    yield CatEvent::Cancelled;
                    yield CatEvent::Done;
                }
                WorkflowRoundOutcome::NoWorkflow => {}
                WorkflowRoundOutcome::Report(report) => {
                    yield CatEvent::Supervisor(report);
                    // A steer can arrive while the supervisor is deciding. If
                    // the decision terminates this workflow node there is no
                    // inner agent stream to drain that queue, so explicitly
                    // inject it into a normal post-workflow turn instead of
                    // dropping it with the registration guard.
                    if let Some(steer) = steer_queue.as_ref() {
                        if let Some(batch) = steer.drain_batch() {
                            yield CatEvent::SteerInjected(crate::SteerInjectedEvent {
                                steer_ids: batch.ids.clone(),
                                session_id: thread_id_owned.clone(),
                                preview: batch.preview.clone(),
                                count: batch.count,
                            });
                            let mut stream = Box::pin(self.stream_with_options(
                                &thread_id_owned,
                                batch.content,
                                opts,
                            ));
                            while let Some(event) = stream.next().await {
                                yield event;
                            }
                        }
                    }
                }
                WorkflowRoundOutcome::Continue { report, message } => {
                    yield CatEvent::Supervisor(report);
                    let mut agent_opts = opts;
                    agent_opts.internal_supervisor_message = true;
                    agent_opts.sender_username = Some("supervisor".to_string());
                    agent_opts.sender_user_id = Some("supervisor".to_string());
                    agent_opts.message_id = None;
                    agent_opts.im_attachments.clear();
                    agent_opts.im_documents.clear();
                    let mut stream = Box::pin(self.stream_with_options(
                        &thread_id_owned,
                        Content::text(message),
                        agent_opts,
                    ));
                    while let Some(event) = stream.next().await {
                        yield event;
                    }
                }
            }
        }
    }

    pub async fn workflow_status(&self, thread_id: &str) -> Option<WorkflowInstance> {
        let user_state = self.memory.load_user_state(thread_id).await;
        if let Some(instance) = supervisor_workflow::instance_from_user_state(&user_state) {
            return Some(instance);
        }
        let instance = goal::migrate_legacy_goal(&self.memory.data_dir, thread_id).await?;
        if self
            .save_workflow_instance(thread_id, &instance)
            .await
            .is_ok()
        {
            let _ = supervisor_workflow::clear_instance(&self.memory.data_dir, thread_id).await;
        }
        Some(instance)
    }

    pub async fn pause_workflow(&self, thread_id: &str) -> Result<(), AgentError> {
        let Some(mut instance) = self.workflow_status(thread_id).await else {
            return Ok(());
        };
        instance.status = WorkflowStatus::Paused;
        instance.pause_reason = Some(supervisor_workflow::WorkflowPauseReason::Manual);
        instance.updated_at = chrono::Utc::now();
        instance.last_report = Some(WorkflowReport {
            workflow_id: instance.definition.id.clone(),
            workflow_name: instance.definition.name.clone(),
            from_node: instance.current_node.clone(),
            edge: None,
            to_node: instance.current_node.clone(),
            path_edges: Vec::new(),
            path_nodes: Vec::new(),
            status: WorkflowStatus::Paused,
            reason: "paused by user".to_string(),
            agent_message: None,
            next_node_message: None,
            supervisor_trace: Vec::new(),
            round: instance
                .last_report
                .as_ref()
                .map(|report| report.round)
                .unwrap_or(0),
            max_rounds: instance.max_rounds.clone(),
            error: None,
        });
        self.save_workflow_instance(thread_id, &instance).await
    }

    async fn pause_workflow_for_user_input(
        &self,
        thread_id: &str,
        round: u32,
    ) -> Option<(WorkflowReport, WorkflowInstance)> {
        let mut instance = self.workflow_status(thread_id).await?;
        if instance.status != WorkflowStatus::Active {
            return None;
        }
        let report = instance.enter_ask_user_for_help(round);
        if let Err(err) = self.save_workflow_instance(thread_id, &instance).await {
            tracing::warn!(thread_id, error = %err, "failed to save workflow user-input pause");
        }
        Some((report, instance))
    }

    async fn resume_workflow_from_user_input(
        &self,
        thread_id: &str,
        round: u32,
    ) -> Option<(WorkflowReport, WorkflowInstance)> {
        let mut instance = self.workflow_status(thread_id).await?;
        let report = instance.resume_from_ask_user_for_help(round)?;
        if let Err(err) = self.save_workflow_instance(thread_id, &instance).await {
            tracing::warn!(thread_id, error = %err, "failed to save workflow user-input resume");
        }
        Some((report, instance))
    }

    async fn pause_workflow_for_user_interruption(
        &self,
        thread_id: &str,
        round: u32,
        reason: &str,
    ) -> Option<(WorkflowReport, WorkflowInstance)> {
        let mut instance = self.workflow_status(thread_id).await?;
        if instance.status != WorkflowStatus::Active {
            return None;
        }
        instance.status = WorkflowStatus::Paused;
        instance.pause_reason = Some(supervisor_workflow::WorkflowPauseReason::Interrupted);
        instance.updated_at = chrono::Utc::now();
        let report = WorkflowReport {
            workflow_id: instance.definition.id.clone(),
            workflow_name: instance.definition.name.clone(),
            from_node: instance.current_node.clone(),
            edge: None,
            to_node: instance.current_node.clone(),
            path_edges: Vec::new(),
            path_nodes: Vec::new(),
            status: WorkflowStatus::Paused,
            reason: reason.to_string(),
            agent_message: None,
            next_node_message: instance.node_message.clone(),
            supervisor_trace: Vec::new(),
            round,
            max_rounds: instance.max_rounds.clone(),
            error: None,
        };
        instance.last_report = Some(report.clone());
        if let Err(err) = self.save_workflow_instance(thread_id, &instance).await {
            tracing::warn!(thread_id, error = %err, "failed to save workflow interruption pause");
        }
        Some((report, instance))
    }

    async fn resume_workflow_from_user_interruption(
        &self,
        thread_id: &str,
        round: u32,
    ) -> Option<(WorkflowReport, WorkflowInstance)> {
        let mut instance = self.workflow_status(thread_id).await?;
        if instance.status != WorkflowStatus::Paused
            || instance.pause_reason != Some(supervisor_workflow::WorkflowPauseReason::Interrupted)
        {
            return None;
        }
        instance.status = WorkflowStatus::Active;
        instance.pause_reason = None;
        instance.updated_at = chrono::Utc::now();
        let report = WorkflowReport {
            workflow_id: instance.definition.id.clone(),
            workflow_name: instance.definition.name.clone(),
            from_node: instance.current_node.clone(),
            edge: Some("interrupted_resume".to_string()),
            to_node: instance.current_node.clone(),
            path_edges: vec!["interrupted_resume".to_string()],
            path_nodes: vec![instance.current_node.clone()],
            status: WorkflowStatus::Active,
            reason: "user input received; resuming workflow".to_string(),
            agent_message: None,
            next_node_message: instance.node_message.clone(),
            supervisor_trace: Vec::new(),
            round,
            max_rounds: instance.max_rounds.clone(),
            error: None,
        };
        instance.last_report = Some(report.clone());
        if let Err(err) = self.save_workflow_instance(thread_id, &instance).await {
            tracing::warn!(thread_id, error = %err, "failed to save workflow interruption resume");
        }
        Some((report, instance))
    }

    pub async fn stop_workflow(&self, thread_id: &str) -> Result<(), AgentError> {
        self.pause_workflow(thread_id).await
    }

    pub async fn clear_workflow(&self, thread_id: &str) -> Result<(), AgentError> {
        let mut user_state = self.memory.load_user_state(thread_id).await;
        supervisor_workflow::remove_instance_from_user_state(&mut user_state);
        self.memory.save_user_state(thread_id, &user_state).await?;
        supervisor_workflow::clear_instance(&self.memory.data_dir, thread_id)
            .await
            .map_err(|err| AgentError::Io(err.to_string()))?;
        goal::clear_legacy_goal(&self.memory.data_dir, thread_id)
            .await
            .map_err(|err| AgentError::Io(err.to_string()))
    }

    async fn save_workflow_instance(
        &self,
        thread_id: &str,
        instance: &WorkflowInstance,
    ) -> Result<(), AgentError> {
        let mut user_state = self.memory.load_user_state(thread_id).await;
        supervisor_workflow::set_instance_in_user_state(&mut user_state, instance)
            .map_err(|err| AgentError::other(format!("serialize supervisor workflow: {err}")))?;
        self.memory.save_user_state(thread_id, &user_state).await
    }

    async fn thread_run_lock(&self, thread_id: &str) -> ThreadRunLock {
        thread_run_lock(&self.run_locks, thread_id).await
    }

    pub async fn is_thread_running(&self, thread_id: &str) -> bool {
        let lock = self.thread_run_lock(thread_id).await;
        lock.try_lock().is_err()
            || (async_agent_enabled_from_env()
                && self.tool_tasks.is_thread_running(thread_id).await)
    }

    pub async fn cancel_background_tasks(&self, thread_id: &str) -> Vec<crate::ToolTaskRecord> {
        self.tool_tasks.cancel_thread(thread_id).await
    }

    pub async fn list_background_tasks(
        &self,
        thread_id: Option<&str>,
    ) -> Vec<crate::ToolTaskRecord> {
        match thread_id {
            Some(thread_id) => self.tool_tasks.list_session_background(thread_id).await,
            None => self.tool_tasks.list(None).await,
        }
    }

    pub async fn get_background_task(&self, task_id: &str) -> Option<crate::ToolTaskRecord> {
        self.tool_tasks.get(task_id).await
    }

    pub async fn cancel_background_task(&self, task_id: &str) -> Option<crate::ToolTaskRecord> {
        self.tool_tasks.cancel(task_id).await
    }

    pub async fn acp_bound_message(
        &self,
        session_id: &str,
        message: &str,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        self.acp_backend
            .continue_bound_session(session_id, message)
            .await
            .map_err(|err| remi_agentloop::prelude::AgentError::tool("codex", err.to_string()))
    }

    pub async fn acp_session_id_for_sub_session(&self, sub_session_id: &str) -> Option<String> {
        self.acp_backend
            .session_id_for_sub_session(sub_session_id)
            .await
    }

    pub async fn acp_binding_status(
        &self,
        platform: &str,
        channel_id: &str,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        self.acp_backend
            .binding_status_text(platform, channel_id)
            .await
            .map_err(|err| remi_agentloop::prelude::AgentError::tool("codex", err.to_string()))
    }

    pub async fn acp_unbind_channel(
        &self,
        platform: &str,
        channel_id: &str,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        let removed = self
            .acp_backend
            .unbind_channel(platform, channel_id)
            .await
            .map_err(|err| remi_agentloop::prelude::AgentError::tool("codex", err.to_string()))?;
        if removed {
            Ok("ACP 绑定已解除。".to_string())
        } else {
            Ok("当前频道没有绑定 ACP session。".to_string())
        }
    }

    /// Rebuild the secret redactor from a new `key → value` map.
    ///
    /// Called by the agent session loop when a `SecretsSync` gRPC message arrives.
    pub fn update_secret_redactor(&self, entries: &std::collections::HashMap<String, String>) {
        let new_redactor = SecretRedactor::from_entries(entries);
        *self.redactor.write().unwrap() = new_redactor;
    }

    /// Return `(name, description)` pairs for every registered tool.
    ///
    /// Useful for the `/tools` slash command — lets users see what the agent
    /// can do without reading source code.
    pub fn tool_list(&self) -> Vec<(String, String)> {
        cat_agent_tool_definitions(&self.inner)
            .into_iter()
            .filter(|d| {
                self.inner
                    .tool_allowlist
                    .as_ref()
                    .map(|allowlist| allowlist.iter().any(|tool| tool == &d.function.name))
                    .unwrap_or(true)
            })
            .map(|d| (d.function.name, d.function.description))
            .collect()
    }

    pub fn tool_list_for_agent(&self, session_agent_id: Option<&str>) -> Vec<(String, String)> {
        self.tool_list_for_agent_and_model(session_agent_id, None)
    }

    pub fn tool_list_for_agent_and_model(
        &self,
        session_agent_id: Option<&str>,
        session_model_profile_id: Option<&str>,
    ) -> Vec<(String, String)> {
        let effective_agent = self.effective_agent_profile(session_agent_id);
        let effective_model = self
            .effective_model_profile_for_agent(session_model_profile_id, &effective_agent.profile);
        let active_agent = self
            .runtime_for_agent_and_model(&effective_agent.profile.id, &effective_model.profile.id);
        cat_agent_tool_definitions(active_agent)
            .into_iter()
            .filter(|d| {
                active_agent
                    .tool_allowlist
                    .as_ref()
                    .map(|allowlist| allowlist.iter().any(|tool| tool == &d.function.name))
                    .unwrap_or(true)
            })
            .map(|d| (d.function.name, d.function.description))
            .collect()
    }

    /// Return every tool known to this runtime, without applying the active
    /// agent allowlist. This is intended for diagnostics and self-management:
    /// an agent can inspect unavailable tools and decide how to update its
    /// profile allowlist or runtime configuration.
    pub fn registered_tool_statuses(&self) -> Vec<RegisteredToolStatus> {
        let enabled_defs = cat_agent_tool_definitions(&self.inner);
        let mut by_name: HashMap<String, RegisteredToolStatus> = HashMap::new();
        for definition in enabled_defs {
            let name = definition.function.name;
            by_name.insert(
                name.clone(),
                RegisteredToolStatus {
                    description: definition.function.description,
                    registered: true,
                    enabled: true,
                    in_active_allowlist: self.tool_in_active_allowlist(&name),
                    warnings: tool_warnings(&name),
                    errors: tool_runtime_errors(&name),
                    name,
                },
            );
        }

        for (name, description) in builtin_tool_catalog() {
            let registered = cat_agent_contains_tool(&self.inner, name);
            by_name
                .entry(name.to_string())
                .or_insert_with(|| RegisteredToolStatus {
                    name: name.to_string(),
                    description: description.to_string(),
                    registered,
                    enabled: registered,
                    in_active_allowlist: self.tool_in_active_allowlist(name),
                    warnings: tool_warnings(name),
                    errors: {
                        let mut errors = tool_errors(name, registered);
                        errors.extend(tool_runtime_errors(name));
                        errors
                    },
                });
        }

        if !self.acp_backend.active_tool_available() {
            let acp_tool_name = self.acp_backend.active_tool_name();
            let acp_tool_description = self.acp_backend.active_tool_description();
            by_name
                .entry(acp_tool_name.clone())
                .and_modify(|entry| {
                    entry.registered = false;
                    entry.enabled = false;
                    entry.errors = tool_errors(&acp_tool_name, false);
                })
                .or_insert_with(|| RegisteredToolStatus {
                    name: acp_tool_name.clone(),
                    description: acp_tool_description,
                    registered: false,
                    enabled: false,
                    in_active_allowlist: self.tool_in_active_allowlist(&acp_tool_name),
                    warnings: Vec::new(),
                    errors: tool_errors(&acp_tool_name, false),
                });
        }

        let mut tools = by_name.into_values().collect::<Vec<_>>();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    fn tool_in_active_allowlist(&self, name: &str) -> bool {
        self.inner
            .tool_allowlist
            .as_ref()
            .map(|allowlist| allowlist.iter().any(|tool| tool == name))
            .unwrap_or(true)
    }

    async fn evaluate_workflow_after_round(
        &self,
        thread_id: &str,
        history: &[Message],
        todo_prompt: Option<String>,
        completed_continuations: u32,
        model_profile_id: Option<&str>,
        cancel: Option<CancellationToken>,
        progress: tokio::sync::mpsc::UnboundedSender<SupervisorTraceEvent>,
    ) -> WorkflowRoundOutcome {
        let Some(mut instance) = self.workflow_status(thread_id).await else {
            return WorkflowRoundOutcome::NoWorkflow;
        };
        if instance.status != WorkflowStatus::Active {
            return WorkflowRoundOutcome::NoWorkflow;
        }
        let from_node = instance.current_node.clone();
        let decision = match self
            .run_supervisor_node(
                thread_id,
                &instance,
                history,
                todo_prompt.as_deref(),
                model_profile_id,
                cancel,
                progress,
            )
            .await
        {
            Ok(SupervisorNodeOutcome::Decision(decision)) => decision,
            Ok(SupervisorNodeOutcome::Cancelled) => {
                if let Some((report, _instance)) = self
                    .pause_workflow_for_user_interruption(
                        thread_id,
                        completed_continuations,
                        "interrupted by user",
                    )
                    .await
                {
                    return WorkflowRoundOutcome::Report(report);
                }
                return WorkflowRoundOutcome::Cancelled;
            }
            Err(err) => {
                return self
                    .workflow_error(
                        thread_id,
                        instance,
                        from_node,
                        completed_continuations,
                        err.to_string(),
                    )
                    .await
            }
        };
        let (report, agent_message) = match supervisor_workflow::apply_decision(
            &mut instance,
            decision,
            completed_continuations,
        ) {
            Ok(result) => result,
            Err(err) => {
                return self
                    .workflow_error(thread_id, instance, from_node, completed_continuations, err)
                    .await;
            }
        };
        if let Err(err) = self.save_workflow_instance(thread_id, &instance).await {
            tracing::warn!(thread_id, error = %err, "failed to save supervisor workflow");
        }
        let Some(message) = agent_message else {
            return WorkflowRoundOutcome::Report(report);
        };
        if !workflow_round_allows_continue(&instance.max_rounds, completed_continuations) {
            return WorkflowRoundOutcome::Report(report);
        }
        WorkflowRoundOutcome::Continue { report, message }
    }

    async fn workflow_error(
        &self,
        thread_id: &str,
        mut instance: WorkflowInstance,
        node: String,
        round: u32,
        error: String,
    ) -> WorkflowRoundOutcome {
        instance.status = WorkflowStatus::Error;
        instance.updated_at = chrono::Utc::now();
        let report = WorkflowReport {
            workflow_id: instance.definition.id.clone(),
            workflow_name: instance.definition.name.clone(),
            from_node: node.clone(),
            edge: None,
            to_node: node,
            path_edges: Vec::new(),
            path_nodes: Vec::new(),
            status: WorkflowStatus::Error,
            reason: error.clone(),
            agent_message: None,
            next_node_message: None,
            supervisor_trace: Vec::new(),
            round,
            max_rounds: instance.max_rounds.clone(),
            error: Some(error),
        };
        instance.last_report = Some(report.clone());
        let _ = self.save_workflow_instance(thread_id, &instance).await;
        WorkflowRoundOutcome::Report(report)
    }

    async fn run_supervisor_node(
        &self,
        thread_id: &str,
        instance: &WorkflowInstance,
        history: &[Message],
        todo_prompt: Option<&str>,
        model_profile_id: Option<&str>,
        cancel: Option<CancellationToken>,
        progress: tokio::sync::mpsc::UnboundedSender<SupervisorTraceEvent>,
    ) -> Result<SupervisorNodeOutcome, AgentError> {
        let effective_model = self.effective_model_profile(model_profile_id);
        let prompt_budget = supervisor_prompt_budget_tokens(&effective_model.profile);
        let prompt = supervisor_workflow::supervisor_prompt_with_budget(
            instance,
            history,
            todo_prompt,
            Some(prompt_budget),
        )
        .map_err(AgentError::other)?;
        if prompt.included_history_messages < prompt.original_history_messages {
            tracing::warn!(
                thread_id,
                estimated_tokens = prompt.estimated_tokens,
                budget_tokens = prompt_budget,
                original_history_messages = prompt.original_history_messages,
                included_history_messages = prompt.included_history_messages,
                "trimmed old main-agent history messages from supervisor prompt"
            );
        }
        let active_agent = self.agent_for_model_profile(&effective_model.profile.id);
        let mut retry_feedback: Option<String> = None;
        let mut combined_trace = Vec::new();
        let mut last_validation_error: Option<String> = None;

        for attempt in 1..=SUPERVISOR_DECISION_MAX_ATTEMPTS {
            if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Ok(SupervisorNodeOutcome::Cancelled);
            }
            let supervisor_thread_id = format!("supervisor:{thread_id}:{}", uuid::Uuid::new_v4());
            let prompt_content = if let Some(feedback) = retry_feedback.as_deref() {
                format!("{}\n\n{}", prompt.content, feedback)
            } else {
                prompt.content.clone()
            };
            let input = LoopInput::start(prompt_content).metadata(serde_json::json!({
                "thread_id": supervisor_thread_id,
                "supervisor_run": "true",
                "supervisor_decision_attempt": attempt,
                "supervisor_decision_max_attempts": SUPERVISOR_DECISION_MAX_ATTEMPTS,
                "supervisor_prompt_estimated_tokens": prompt.estimated_tokens,
                "supervisor_prompt_budget_tokens": prompt_budget,
                "supervisor_history_original_messages": prompt.original_history_messages,
                "supervisor_history_included_messages": prompt.included_history_messages,
            }));
            let output = tokio::time::timeout(SUPERVISOR_TIMEOUT, async {
                let mut stream = std::pin::pin!(active_agent
                    .stream_with_input_and_tool_allowlist_and_options(
                        input,
                        // Supervisor decisions are made solely from the
                        // workflow payload and main-agent history. Do not
                        // expose runtime, dynamic, or model-provided tools.
                        Some(Vec::new()),
                        CoreStreamOptions {
                            cancel: cancel.clone(),
                            steer: None,
                            async_agent: false,
                        },
                    ));
                let mut output = String::new();
                let mut trace = Vec::new();
                let mut tool_args_by_id: std::collections::HashMap<String, serde_json::Value> =
                    std::collections::HashMap::new();
                loop {
                    let event = match cancel.as_ref() {
                        Some(cancel) => {
                            tokio::select! {
                                _ = cancel.cancelled() => return Ok(None),
                                event = stream.next() => event,
                            }
                        }
                        None => stream.next().await,
                    };
                    let Some(event) = event else {
                        break;
                    };
                    match event {
                        CatEvent::Cancelled => return Ok(None),
                        CatEvent::Text(delta) => {
                            output.push_str(&delta);
                            let _ =
                                progress.send(SupervisorTraceEvent::OutputDelta { content: delta });
                        }
                        CatEvent::Thinking(content) => {
                            let event = SupervisorTraceEvent::Thinking { content };
                            let _ = progress.send(event.clone());
                            trace.push(event);
                        }
                        CatEvent::ToolCallStart { id, name } => {
                            let event = SupervisorTraceEvent::ToolCallStart { id, name };
                            let _ = progress.send(event.clone());
                            trace.push(event);
                        }
                        CatEvent::ToolCallArgumentsDelta { id, delta } => {
                            let event = SupervisorTraceEvent::ToolCallArgumentsDelta { id, delta };
                            let _ = progress.send(event.clone());
                            trace.push(event);
                        }
                        CatEvent::ToolCall { id, name, args } => {
                            tool_args_by_id.insert(id.clone(), args.clone());
                            let event = SupervisorTraceEvent::ToolCall { id, name, args };
                            let _ = progress.send(event.clone());
                            trace.push(event);
                        }
                        CatEvent::ToolCallResult {
                            id, name, result, ..
                        } => {
                            let args = tool_args_by_id
                                .remove(&id)
                                .unwrap_or(serde_json::Value::Null);
                            let event = SupervisorTraceEvent::ToolResult {
                                id,
                                name,
                                args,
                                result,
                            };
                            let _ = progress.send(event.clone());
                            trace.push(event);
                        }
                        CatEvent::Error(err) => {
                            return Err(AgentError::other(format!(
                                "{err}\n\nsupervisor trace: {}",
                                serde_json::to_string(&trace).unwrap_or_default()
                            )));
                        }
                        CatEvent::Done => break,
                        _ => {}
                    }
                }
                Ok(Some((output, trace)))
            })
            .await
            .map_err(|_| {
                AgentError::other("supervisor evaluation timed out after 180 seconds")
            })??;
            let Some((output, trace)) = output else {
                return Ok(SupervisorNodeOutcome::Cancelled);
            };
            combined_trace.extend(trace);

            let mut decision = match supervisor_workflow::parse_decision(&output) {
                Ok(decision) => decision,
                Err(err) => {
                    last_validation_error = Some(err.clone());
                    let event = SupervisorTraceEvent::Output {
                        content: format!(
                            "Invalid supervisor decision attempt {attempt}/{SUPERVISOR_DECISION_MAX_ATTEMPTS}: {err}"
                        ),
                    };
                    let _ = progress.send(event.clone());
                    combined_trace.push(event);
                    retry_feedback = Some(supervisor_retry_feedback(&err, &output));
                    continue;
                }
            };
            let mut dry_run = instance.clone();
            if let Err(err) = supervisor_workflow::apply_decision(&mut dry_run, decision.clone(), 0)
            {
                last_validation_error = Some(err.clone());
                let event = SupervisorTraceEvent::Output {
                    content: format!(
                        "Invalid supervisor decision attempt {attempt}/{SUPERVISOR_DECISION_MAX_ATTEMPTS}: {err}"
                    ),
                };
                let _ = progress.send(event.clone());
                combined_trace.push(event);
                retry_feedback = Some(supervisor_retry_feedback(&err, &output));
                continue;
            }

            let pretty = serde_json::to_string_pretty(&decision)
                .map_err(|err| AgentError::other(format!("format supervisor JSON: {err}")))?;
            let final_output = SupervisorTraceEvent::Output { content: pretty };
            let _ = progress.send(final_output.clone());
            combined_trace.push(final_output);
            if let Some(agent_message) = decision.agent_message.as_deref() {
                let event = SupervisorTraceEvent::AgentMessage {
                    content: agent_message.to_string(),
                };
                let _ = progress.send(event.clone());
                combined_trace.push(event);
            }
            decision.trace = combined_trace;
            return Ok(SupervisorNodeOutcome::Decision(decision));
        }

        Err(AgentError::other(format!(
            "supervisor decision remained invalid after {SUPERVISOR_DECISION_MAX_ATTEMPTS} attempts: {}",
            last_validation_error.unwrap_or_else(|| "unknown validation error".to_string())
        )))
    }

    /// Stream events for one conversation turn (text input).
    ///
    /// `thread_id` scopes the memory (use the Feishu `chat_id`).
    pub fn stream<'a>(
        &'a self,
        thread_id: &'a str,
        text: impl Into<String>,
    ) -> impl Stream<Item = CatEvent> + 'a {
        self.stream_content(thread_id, Content::text(text.into()))
    }

    /// Stream events for one conversation turn (arbitrary content — text, images, etc.).
    pub fn stream_content<'a>(
        &'a self,
        thread_id: &'a str,
        content: Content,
    ) -> impl Stream<Item = CatEvent> + 'a {
        self.stream_with_options(thread_id, content, StreamOptions::default())
    }

    /// Stream events with per-turn metadata (sender identity, chat type, etc.).
    ///
    /// `sender_user_id` and `message_id` are stored as metadata on the user
    /// message so they persist in conversation history without polluting the
    /// message body or adding standalone messages.
    pub fn stream_with_options<'a>(
        &'a self,
        thread_id: &'a str,
        content: Content,
        opts: StreamOptions,
    ) -> impl Stream<Item = CatEvent> + 'a {
        let thread_id_owned = thread_id.to_string();
        stream! {
                let run_lock = self.thread_run_lock(&thread_id_owned).await;
                let _run_guard = run_lock.lock().await;
                let steer_registration = if opts.supervisor_run {
                    None
                } else {
                    let (queue, guard) = self.register_steer_queue(&thread_id_owned);
                    Some((queue, guard))
                };
                let steer_queue = steer_registration.as_ref().map(|(queue, _)| Arc::clone(queue));
                let mut next_content = content;
                let mut supervisor_round: u32 = 0;
                let mut continuation_from_supervisor = false;
                let mut continuation_from_background_task = false;
                let mut stop_hook_active = false;
                let mut interruption_resume_checked = false;
                let turn_started = Instant::now();

                'workflow_loop: loop {
                if !interruption_resume_checked && !opts.supervisor_run {
                    interruption_resume_checked = true;
                    if let Some((report, _instance)) = self
                        .resume_workflow_from_user_interruption(&thread_id_owned, supervisor_round)
                        .await
                    {
                        yield CatEvent::Supervisor(report);
                    }
                }
                let workflow_agent_id = self
                    .workflow_status(&thread_id_owned)
                    .await
                    .and_then(|instance| {
                        self.workflow_node_agent(&instance)
                            .map(ToOwned::to_owned)
                    });
                let effective_agent = self.effective_agent_profile_for_workflow(
                    opts.agent_id.as_deref(),
                    workflow_agent_id.as_deref(),
                );
                if effective_agent.invalid_session_agent.is_some() && workflow_agent_id.is_some() {
                    let invalid = effective_agent.invalid_session_agent.clone().unwrap_or_default();
                    let err = AgentError::other(format!(
                        "active workflow node references unknown agent `{invalid}`"
                    ));
                    tracing::warn!(
                        thread_id = %thread_id_owned,
                        elapsed_ms = turn_started.elapsed().as_millis() as u64,
                        error = %err,
                        "agent_turn.failed"
                    );
                    yield CatEvent::Error(err);
                    return;
                }
                let effective_model = self.effective_model_profile_for_agent(
                    opts.model_profile_id.as_deref(),
                    &effective_agent.profile,
                );
                let effective_model = match apply_reasoning_effort_override(
                    effective_model,
                    opts.reasoning_effort,
                ) {
                    Ok(effective) => effective,
                    Err(err) => {
                        let err = AgentError::other(err.to_string());
                        tracing::warn!(
                            thread_id = %thread_id_owned,
                            elapsed_ms = turn_started.elapsed().as_millis() as u64,
                            error = %err,
                            "agent_turn.failed"
                        );
                        yield CatEvent::Error(err);
                        return;
                    }
                };
                let runtime_model_key =
                    model_runtime_key(&effective_model.profile.id, opts.reasoning_effort);
                let active_agent = self.runtime_for_agent_and_model(
                    &effective_agent.profile.id,
                    &runtime_model_key,
                );
                tracing::info!(
                    thread_id = %thread_id_owned,
                    model_profile = %effective_model.profile.id,
                    model = %effective_model.profile.model,
                    reasoning_effort = opts.reasoning_effort.map(ReasoningEffort::as_str).unwrap_or(""),
                    model_source = ?effective_model.source,
                    agent_id = %effective_agent.profile.id,
                    agent_source = ?effective_agent.source,
                    workflow_agent_id = workflow_agent_id.as_deref().unwrap_or(""),
                    platform = opts.platform.as_deref().unwrap_or(""),
                    chat_type = opts.chat_type.as_deref().unwrap_or(""),
                    message_id = opts.message_id.as_deref().unwrap_or(""),
                    sender_user_id = opts.sender_user_id.as_deref().unwrap_or(""),
                    supervisor_run = opts.supervisor_run,
                    "agent_turn.start"
                );
                // 1. Load memory context (triggers mid->long-term promotion if needed).
                let mut ctx = match self.memory.load_context(&thread_id_owned).await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            thread_id = %thread_id_owned,
                            model_profile = %effective_model.profile.id,
                            elapsed_ms = turn_started.elapsed().as_millis() as u64,
                            error = %e,
                            "agent_turn.failed"
                        );
                        yield CatEvent::Error(e);
                        return;
                    }
                };

                if let Err(err) = self
                    .todo_backend
                    .refresh_thread_user_state(
                        &thread_id_owned,
                        opts.sender_user_id.as_deref(),
                        &mut ctx.user_state,
                    )
                    .await
                {
                    tracing::warn!(
                        thread_id = %thread_id_owned,
                        error = %err,
                        "failed to refresh todo state before turn"
                    );
                }

                let environment_context = ensure_environment_context(
                    &mut ctx.user_state,
                    &self.environment_context_source,
                );
                persist_new_environment_context(
                    &self.memory,
                    &thread_id_owned,
                    &ctx.user_state,
                    environment_context.initialized,
                )
                .await;

                let mut round_opts = opts.clone();
                let background_task_continuation = continuation_from_background_task;
                if continuation_from_supervisor || background_task_continuation {
                    if continuation_from_supervisor {
                        round_opts.sender_username = Some("supervisor".to_string());
                        round_opts.sender_user_id = Some("supervisor".to_string());
                    } else {
                        round_opts.sender_username = None;
                        round_opts.sender_user_id = None;
                    }
                    round_opts.message_id = None;
                    round_opts.im_attachments.clear();
                    round_opts.im_documents.clear();
                }
                continuation_from_background_task = false;

                apply_skill_injections(&mut ctx.user_state, &round_opts.skill_injections);
                let requested_user_name = round_opts
                    .sender_username
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let injected_user_name = requested_user_name
                    .as_deref()
                    .and_then(|value| truncate_user_name(Some(value), 10));
                let single_chat_sender_prompt = single_chat_sender_system_prompt(
                    round_opts.chat_type.as_deref(),
                    requested_user_name.as_deref(),
                    round_opts.sender_user_id.as_deref(),
                );

                // 2. Build injected history prefix; record its length to strip later.
                let mut history = build_injected_history(&ctx);
                history.insert(
                    0,
                    Message::system(effective_agent.profile.system_prompt.clone()),
                );
                insert_environment_context_prompt(
                    &mut history,
                    1,
                    environment_context.prompt.clone(),
                );
                let agent_header_count =
                    2 + usize::from(ctx.agent_md.is_some()) + usize::from(ctx.soul_md.is_some());
                let skill_prompt_tools = skill_prompt_tool_availability(active_agent);
                insert_skill_injection_prompts(
                    &mut history,
                    agent_header_count,
                    &round_opts.skill_injections,
                    skill_prompt_tools,
                );
                insert_pinned_skill_prompt(
                    &mut history,
                    agent_header_count,
                    &self.pinned_skill_summaries,
                    effective_model.profile.context_tokens,
                    skill_prompt_tools,
                );
                if round_opts.async_agent {
                    insert_async_tool_system_prompt(
                        &mut history,
                        agent_header_count
                            + round_opts.skill_injections.len()
                            + usize::from(!self.pinned_skill_summaries.is_empty()),
                    );
                }
                insert_single_chat_sender_system_prompt(
                    &mut history,
                    agent_header_count,
                    single_chat_sender_prompt.clone(),
                );
                let active_supervisor = self
                    .workflow_status(&thread_id_owned)
                    .await
                    .is_some_and(|instance| instance.status == WorkflowStatus::Active);
                let initial_supervisor_todo_prompt =
                    route_thread_todo_prompt(&mut history, &ctx.user_state, active_supervisor);
                let mut skip_count;

                // 3. Build request-level metadata (thread_id for tools);
                //    build per-message metadata (sender identity + message id).
                let mut meta = serde_json::json!({ "thread_id": &thread_id_owned });
                if let Some(ref ct) = round_opts.chat_type {
                    meta["chat_type"] = serde_json::Value::String(ct.clone());
                }
                if let Some(ref platform) = round_opts.platform {
                    meta["platform"] = serde_json::Value::String(platform.clone());
                }
                if round_opts.supervisor_run {
                    meta["supervisor_run"] = serde_json::Value::String("true".to_string());
                }

                let mut msg_meta = serde_json::Map::new();
                if round_opts.internal_supervisor_message {
                    msg_meta.insert(
                        "internal_supervisor_message".into(),
                        serde_json::Value::Bool(true),
                    );
                }
                if background_task_continuation {
                    msg_meta.insert(
                        "background_tool_task".into(),
                        serde_json::Value::Bool(true),
                    );
                }
                if let Some(ref sid) = round_opts.sender_user_id {
                    msg_meta.insert("sender_user_id".into(), serde_json::Value::String(sid.clone()));
                    meta["sender_user_id"] = serde_json::Value::String(sid.clone());
                }
                if let Some(ref username) = round_opts.sender_username {
                    let username = username.trim();
                    if !username.is_empty() {
                        let username = username.to_string();
                        msg_meta.insert("sender_username".into(), serde_json::Value::String(username.clone()));
                        meta["sender_username"] = serde_json::Value::String(username);
                    }
                }
                if let Some(ref mid) = round_opts.message_id {
                    msg_meta.insert("message_id".into(), serde_json::Value::String(mid.clone()));
                    meta["message_id"] = serde_json::Value::String(mid.clone());
                }
                if let Some(ref ct) = round_opts.chat_type {
                    msg_meta.insert("chat_type".into(), serde_json::Value::String(ct.clone()));
                }
                if let Some(ref platform) = round_opts.platform {
                    msg_meta.insert("platform".into(), serde_json::Value::String(platform.clone()));
                }
                if !round_opts.im_attachments.is_empty() {
                    let json_str = serde_json::to_string(&round_opts.im_attachments).unwrap_or_default();
                    let str_val = serde_json::Value::String(json_str);
                    msg_meta.insert("im_attachments".into(), str_val.clone());
                    meta["im_attachments"] = str_val;
                }
                if !round_opts.im_documents.is_empty() {
                    let json_str = serde_json::to_string(&round_opts.im_documents).unwrap_or_default();
                    let str_val = serde_json::Value::String(json_str);
                    msg_meta.insert("im_documents".into(), str_val.clone());
                    meta["im_documents"] = str_val;
                }
                let message_metadata = if msg_meta.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::Object(msg_meta))
                };
                let content = prepend_group_sender_username(
                    next_content.clone(),
                    round_opts.chat_type.as_deref(),
                    requested_user_name.as_deref(),
                );

                let pre_hook_history_len = history.len();
                let hook_context = self.hook_context(
                    &thread_id_owned,
                    Some(effective_model.profile.model.clone()),
                    round_opts.message_id.clone(),
                );
                if supervisor_round == 0 && !continuation_from_supervisor {
                    let session_hook = self
                        .hook_manager
                        .run(
                            HookEventName::SessionStart,
                            Some("startup"),
                            &hook_context,
                            serde_json::json!({ "source": "startup" }),
                        )
                        .await;
                    if session_hook.blocked || session_hook.continue_flow == Some(false) {
                        yield CatEvent::Error(AgentError::other(
                            session_hook
                                .reason
                                .unwrap_or_else(|| "session start blocked by hook".to_string()),
                        ));
                        return;
                    }
                    for context in session_hook.additional_context {
                        history.push(hook_context_message("SessionStart", context));
                    }
                }
                if !continuation_from_supervisor && !background_task_continuation {
                    let user_prompt_hook = self
                        .hook_manager
                        .run(
                            HookEventName::UserPromptSubmit,
                            None,
                            &hook_context,
                            serde_json::json!({ "prompt": content.text_content() }),
                        )
                        .await;
                    if user_prompt_hook.blocked || user_prompt_hook.continue_flow == Some(false) {
                        yield CatEvent::Error(AgentError::other(
                            user_prompt_hook
                                .reason
                                .unwrap_or_else(|| "user prompt blocked by hook".to_string()),
                        ));
                        return;
                    }
                    for context in user_prompt_hook.additional_context {
                        history.push(hook_context_message("UserPromptSubmit", context));
                    }
                }
                let hook_messages = history[pre_hook_history_len..].to_vec();
                skip_count = history.len();

                let should_log_media_input = content.is_multimodal()
                    || !round_opts.im_attachments.is_empty()
                    || !round_opts.im_documents.is_empty();
                if should_log_media_input && !effective_model.profile.supports_images {
                    let err = AgentError::other(format!(
                        "current model profile `{}` does not support image/document inputs; switch REMI_MODEL_PROFILE to a multimodal model",
                        effective_model.profile.id
                    ));
                    tracing::warn!(
                        thread_id = %thread_id_owned,
                        model_profile = %effective_model.profile.id,
                        model = %effective_model.profile.model,
                        elapsed_ms = turn_started.elapsed().as_millis() as u64,
                        error = %err,
                        "agent_turn.failed"
                    );
                    yield CatEvent::Error(err);
                    return;
                }
                if should_log_media_input {
                    tracing::info!(
                        thread_id = %thread_id_owned,
                        sender_user_id = opts.sender_user_id.as_deref().unwrap_or(""),
                        message_id = opts.message_id.as_deref().unwrap_or(""),
                        chat_type = opts.chat_type.as_deref().unwrap_or(""),
                        content_summary = %summarize_content_for_log(&content),
                        attachment_count = round_opts.im_attachments.len(),
                        document_count = round_opts.im_documents.len(),
                        "stream_with_options: media input"
                    );
                }

                tracing::debug!(
                    thread_id = %thread_id_owned,
                    skip_count,
                    has_message_metadata = message_metadata.is_some(),
                    has_single_chat_sender_prompt = is_direct_chat(opts.chat_type.as_deref()) && (requested_user_name.is_some() || opts.sender_user_id.as_deref().is_some_and(|value| !value.trim().is_empty())),
                    requested_user_name = requested_user_name.as_deref().unwrap_or(""),
                    injected_user_name = injected_user_name.as_deref().unwrap_or(""),
                    ?message_metadata,
                    "stream_with_options: building LoopInput"
                );
                tracing::info!(
                    thread_id = %thread_id_owned,
                    sender_user_id = round_opts.sender_user_id.as_deref().unwrap_or(""),
                    message_id = round_opts.message_id.as_deref().unwrap_or(""),
                    sender_username = requested_user_name.as_deref().unwrap_or(""),
                    injected_user_name = injected_user_name.as_deref().unwrap_or(""),
                    has_single_chat_sender_prompt = is_direct_chat(round_opts.chat_type.as_deref()) && (requested_user_name.is_some() || round_opts.sender_user_id.as_deref().is_some_and(|value| !value.trim().is_empty())),
                    has_sender_username = requested_user_name.is_some(),
                    has_message_metadata = message_metadata.is_some(),
                    "stream_with_options: username propagation"
                );

                let initial_user_state = ctx.user_state.clone();

                let current_user_message = Message {
                    id: MessageId::new(),
                    role: Role::User,
                    content: content.clone(),
                    tool_calls: None,
                    tool_call_id: None,
                    name: injected_user_name.clone(),
                    reasoning_content: None,
                    metadata: message_metadata.clone(),
                };
                let mut partial_base_history = history.clone();
                partial_base_history.push(current_user_message.clone());
                let mut partial_turn = PartialTurnRecorder::new(partial_base_history);

                let mut input = LoopInput::start_content(content.clone())
                    .history(history)
                    .metadata(meta.clone())
                    .user_state(ctx.user_state);
                if let Some(user_name) = injected_user_name.clone() {
                    input = input.user_name(user_name);
                }
                if let Some(mm) = message_metadata.clone() {
                    input = input.message_metadata(mm);
                }

                let request_budget = model_request_budget_tokens(
                    &effective_model.profile,
                    auto_compress_context_percent().unwrap_or(DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT),
                );
                let final_snapshot = loop {
                    let request_tool_definitions =
                        active_agent.tool_definitions_for_input(&input, None);
                    let snapshot = model_input_snapshot_from_loop_input(
                        &input,
                        &request_tool_definitions,
                        &thread_id_owned,
                        round_opts.message_id.as_deref(),
                        &effective_model.profile.id,
                        &effective_model.profile.model,
                    );
                    let Some(snapshot) = snapshot else { break None };
                    if snapshot.totals.estimated_tokens <= request_budget {
                        break Some(snapshot);
                    }
                    if !self.memory.auto_compress {
                        let _ = self
                            .memory
                            .append_failed_turn(&thread_id_owned, vec![current_user_message.clone()])
                            .await;
                        yield CatEvent::Error(AgentError::other(format!(
                            "model request exceeds context budget: estimated {} tokens, limit {} tokens; automatic compression is disabled",
                            snapshot.totals.estimated_tokens, request_budget
                        )));
                        return;
                    }
                    let compacted = match self.memory.compact_for_request(&thread_id_owned).await {
                        Ok(count) if count > 0 => count,
                        Ok(_) => {
                            let _ = self
                                .memory
                                .append_failed_turn(&thread_id_owned, vec![current_user_message.clone()])
                                .await;
                            yield CatEvent::Error(AgentError::other(format!(
                                "model request exceeds context budget: estimated {} tokens, limit {} tokens; no complete older exchange is eligible for compression",
                                snapshot.totals.estimated_tokens, request_budget
                            )));
                            return;
                        }
                        Err(err) => {
                            let _ = self
                                .memory
                                .append_failed_turn(&thread_id_owned, vec![current_user_message.clone()])
                                .await;
                            yield CatEvent::Error(AgentError::other(format!(
                                "pre-request memory compression failed while reducing {} estimated tokens to the {} token limit: {err}",
                                snapshot.totals.estimated_tokens, request_budget
                            )));
                            return;
                        }
                    };
                    tracing::info!(
                        thread_id = %thread_id_owned,
                        compacted_messages = compacted,
                        previous_estimated_tokens = snapshot.totals.estimated_tokens,
                        request_budget,
                        "pre-request memory compression completed"
                    );

                    let mut refreshed = match self.memory.load_context(&thread_id_owned).await {
                        Ok(value) => value,
                        Err(err) => {
                            let _ = self
                                .memory
                                .append_failed_turn(&thread_id_owned, vec![current_user_message.clone()])
                                .await;
                            yield CatEvent::Error(AgentError::other(format!(
                                "reload memory after pre-request compression: {err}"
                            )));
                            return;
                        }
                    };
                    refreshed.user_state = initial_user_state.clone();
                    let mut rebuilt = build_injected_history(&refreshed);
                    rebuilt.insert(0, Message::system(effective_agent.profile.system_prompt.clone()));
                    insert_environment_context_prompt(&mut rebuilt, 1, environment_context.prompt.clone());
                    insert_skill_injection_prompts(
                        &mut rebuilt,
                        agent_header_count,
                        &round_opts.skill_injections,
                        skill_prompt_tools,
                    );
                    insert_pinned_skill_prompt(
                        &mut rebuilt,
                        agent_header_count,
                        &self.pinned_skill_summaries,
                        effective_model.profile.context_tokens,
                        skill_prompt_tools,
                    );
                    if round_opts.async_agent {
                        insert_async_tool_system_prompt(
                            &mut rebuilt,
                            agent_header_count
                                + round_opts.skill_injections.len()
                                + usize::from(!self.pinned_skill_summaries.is_empty()),
                        );
                    }
                    insert_single_chat_sender_system_prompt(
                        &mut rebuilt,
                        agent_header_count,
                        single_chat_sender_prompt.clone(),
                    );
                    route_thread_todo_prompt(&mut rebuilt, &refreshed.user_state, active_supervisor);
                    rebuilt.extend(hook_messages.clone());
                    skip_count = rebuilt.len();
                    input = LoopInput::start_content(content.clone())
                        .history(rebuilt)
                        .metadata(meta.clone())
                        .user_state(initial_user_state.clone());
                    if let Some(user_name) = injected_user_name.clone() {
                        input = input.user_name(user_name);
                    }
                    if let Some(mm) = message_metadata.clone() {
                        input = input.message_metadata(mm);
                    }
                };
                if let Some(snapshot) = final_snapshot {
                    yield CatEvent::ModelInputSnapshot(snapshot);
                }

                yield CatEvent::StateUpdate(initial_user_state.clone());

                // 4. Drive inner agent, intercept History event to persist.
                let mut raw_history: Option<Vec<Message>> = None;
                let mut raw_user_state: Option<serde_json::Value> = None;
                let mut tool_elapsed_ms = HashMap::<String, u64>::new();
                let mut user_question_requested_this_round = false;
                let mut workflow_interrupted_this_round = false;
                let mut workflow_user_state_override: Option<WorkflowInstance> = None;
                let inner_stream = active_agent.stream_with_input_and_options(
                    input,
                    CoreStreamOptions {
                        cancel: round_opts.cancel.clone(),
                        steer: steer_queue.clone(),
                        async_agent: round_opts.async_agent,
                    },
                );
                let mut inner_stream = std::pin::pin!(inner_stream);
                let mut completed_tool_tasks = self.tool_tasks.subscribe_completed(&thread_id_owned).await;
                let mut background_side_events = self.tool_tasks.subscribe_side_events(&thread_id_owned).await;

                loop {
                    let ev = tokio::select! {
                        ev = inner_stream.next() => {
                            let Some(ev) = ev else {
                                break;
                            };
                            ev
                        }
                        completed = completed_tool_tasks.recv() => {
                            if let Ok(task) = completed {
                                if task.thread_id == thread_id_owned
                                    && self.tool_tasks.claim_completion_notification(&task.task_id).await
                                {
                                    if let Some(steer) = steer_queue.as_ref() {
                                        steer.push(background_task_completion_steer_input(&task));
                                    }
                                    yield CatEvent::ToolTaskCompleted(task);
                                }
                            }
                            continue;
                        }
                        side_event = background_side_events.recv() => {
                            if let Ok((event_thread_id, event)) = side_event {
                                if event_thread_id == thread_id_owned {
                                    yield event;
                                }
                            }
                            continue;
                        }
                    };
                    match ev {
                        CatEvent::Cancelled => {
                            let cancelled_tasks =
                                self.cancel_background_tasks(&thread_id_owned).await;
                            if !cancelled_tasks.is_empty() {
                                tracing::info!(
                                    thread_id = %thread_id_owned,
                                    cancelled_task_count = cancelled_tasks.len(),
                                    "agent_turn.cancelled_background_tasks"
                                );
                            }
                            tracing::info!(
                                thread_id = %thread_id_owned,
                                model_profile = %effective_model.profile.id,
                                elapsed_ms = turn_started.elapsed().as_millis() as u64,
                                "agent_turn.cancelled"
                            );
                            if !round_opts.supervisor_run {
                                if let Some((report, instance)) = self
                                    .pause_workflow_for_user_interruption(
                                        &thread_id_owned,
                                        supervisor_round,
                                        "interrupted by user",
                                    )
                                    .await
                                {
                                    workflow_user_state_override = Some(instance);
                                    if raw_user_state.is_none() {
                                        raw_user_state = Some(initial_user_state.clone());
                                    }
                                    if let Some(user_state) = raw_user_state.as_mut() {
                                        if let Some(instance) = workflow_user_state_override.as_ref() {
                                            let _ = supervisor_workflow::set_instance_in_user_state(
                                                user_state,
                                                instance,
                                            );
                                        }
                                    }
                                    yield CatEvent::Supervisor(report);
                                }
                            }
                            for event in persist_turn(
                                &self.memory, &thread_id_owned,
                                raw_history.take().or_else(|| partial_turn.synthesize_history()),
                                raw_user_state.take().or_else(|| Some(initial_user_state.clone())),
                                skip_count, &tool_elapsed_ms,
                            ).await {
                                yield event;
                            }
                            yield CatEvent::Done;
                            return;
                        }
                        CatEvent::UserInterrupted { reason } => {
                            if !round_opts.supervisor_run {
                                workflow_interrupted_this_round = true;
                                if let Some((report, instance)) = self
                                    .pause_workflow_for_user_interruption(
                                        &thread_id_owned,
                                        supervisor_round,
                                        &reason,
                                    )
                                    .await
                                {
                                    workflow_user_state_override = Some(instance);
                                    if raw_user_state.is_none() {
                                        raw_user_state = Some(initial_user_state.clone());
                                    }
                                    if let Some(user_state) = raw_user_state.as_mut() {
                                        if let Some(instance) = workflow_user_state_override.as_ref() {
                                            let _ = supervisor_workflow::set_instance_in_user_state(
                                                user_state,
                                                instance,
                                            );
                                        }
                                    }
                                    yield CatEvent::Supervisor(report);
                                }
                            }
                        }
                        CatEvent::History(msgs, mut us) => {
                            if workflow_user_state_override.is_some() {
                                if let Some(instance) = self.workflow_status(&thread_id_owned).await {
                                    workflow_user_state_override = Some(instance);
                                }
                            }
                            if let Some(instance) = workflow_user_state_override.as_ref() {
                                if let Err(err) =
                                    supervisor_workflow::set_instance_in_user_state(&mut us, instance)
                                {
                                    tracing::warn!(
                                        thread_id = %thread_id_owned,
                                        error = %err,
                                        "failed to merge workflow override into history user_state"
                                    );
                                }
                            }
                            raw_history = Some(msgs);
                            raw_user_state = Some(us);
                        }
                            CatEvent::Text(text) => {
                                partial_turn.on_text(&text);
                                yield CatEvent::Text(text);
                            }
                            CatEvent::Thinking(content) => {
                                partial_turn.on_thinking(content.clone());
                                yield CatEvent::Thinking(content);
                            }
                            CatEvent::ToolCallStart { id, name } => {
                                partial_turn.on_tool_start(id.clone(), name.clone());
                                yield CatEvent::ToolCallStart { id, name };
                            }
                            CatEvent::ToolCallArgumentsDelta { id, delta } => {
                                partial_turn.on_tool_arguments_delta(&id, &delta);
                                yield CatEvent::ToolCallArgumentsDelta { id, delta };
                            }
                            CatEvent::ToolCall { id, name, args } => {
                                partial_turn.on_tool_call(id.clone(), name.clone(), args.clone());
                                yield CatEvent::ToolCall { id, name, args };
                            }
                            // Persist user_state immediately after each tool round.
                            CatEvent::StateUpdate(mut us) => {
                                if workflow_user_state_override.is_some() {
                                    if let Some(instance) = self.workflow_status(&thread_id_owned).await {
                                        workflow_user_state_override = Some(instance);
                                    }
                                }
                                if let Some(instance) = workflow_user_state_override.as_ref() {
                                    if let Err(err) =
                                        supervisor_workflow::set_instance_in_user_state(&mut us, instance)
                                    {
                                        tracing::warn!(
                                            thread_id = %thread_id_owned,
                                            error = %err,
                                            "failed to merge workflow override into user_state"
                                        );
                                    }
                                }
                                raw_user_state = Some(us.clone());
                                yield persist_intermediate_user_state(
                                    &self.memory,
                                    &thread_id_owned,
                                    us,
                                )
                                .await;
                            }
                            CatEvent::ToolCallResult {
                                id,
                                name,
                                args,
                                result,
                                success,
                                elapsed_ms,
                            } => {
                                tool_elapsed_ms.insert(id.clone(), elapsed_ms);
                                partial_turn.on_tool_result(
                                    id.clone(),
                                    name.clone(),
                                    args.clone(),
                                    result.clone(),
                                    success,
                                    elapsed_ms,
                                );
                                yield CatEvent::ToolCallResult {
                                    id,
                                    name,
                                    args,
                                    result,
                                    success,
                                    elapsed_ms,
                                };
                            }
                            CatEvent::ToolApprovalRequested(request) => {
                                yield CatEvent::ToolApprovalRequested(request);
                            }
                            CatEvent::ToolApprovalUpdated(request) => {
                                yield CatEvent::ToolApprovalUpdated(request);
                            }
                            CatEvent::ToolApprovalResolved { request, decision } => {
                                yield CatEvent::ToolApprovalResolved { request, decision };
                            }
                            CatEvent::UserQuestionRequested(request) => {
                                if !round_opts.supervisor_run {
                                    user_question_requested_this_round = true;
                                    if let Some((report, instance)) = self
                                        .pause_workflow_for_user_input(
                                            &thread_id_owned,
                                            supervisor_round,
                                        )
                                        .await
                                    {
                                        workflow_user_state_override = Some(instance);
                                        if let Some(user_state) = raw_user_state.as_mut() {
                                            if let Some(instance) = workflow_user_state_override.as_ref() {
                                                let _ = supervisor_workflow::set_instance_in_user_state(
                                                    user_state,
                                                    instance,
                                                );
                                            }
                                        }
                                        yield CatEvent::Supervisor(report);
                                    }
                                }
                                yield CatEvent::UserQuestionRequested(request);
                            }
                            CatEvent::UserQuestionUpdated(request) => {
                                yield CatEvent::UserQuestionUpdated(request);
                            }
                            CatEvent::UserQuestionResolved { request, response } => {
                                if !round_opts.supervisor_run {
                                    if let Some((report, instance)) = self
                                        .resume_workflow_from_user_input(
                                            &thread_id_owned,
                                            supervisor_round,
                                        )
                                        .await
                                    {
                                        workflow_user_state_override = Some(instance);
                                        if let Some(user_state) = raw_user_state.as_mut() {
                                            if let Some(instance) = workflow_user_state_override.as_ref() {
                                                let _ = supervisor_workflow::set_instance_in_user_state(
                                                    user_state,
                                                    instance,
                                                );
                                            }
                                        }
                                        yield CatEvent::Supervisor(report);
                                    }
                                }
                                yield CatEvent::UserQuestionResolved { request, response };
                            }
                            // Save memory BEFORE yielding Done/Error — the caller drops
                            // the stream immediately on these events, so any code after
                            // this loop would never execute.
                            CatEvent::Done => {
                                if workflow_user_state_override.is_some() {
                                    if let Some(instance) = self.workflow_status(&thread_id_owned).await
                                    {
                                        workflow_user_state_override = Some(instance);
                                    }
                                    if let (Some(user_state), Some(instance)) =
                                        (raw_user_state.as_mut(), workflow_user_state_override.as_ref())
                                    {
                                        if let Err(err) =
                                            supervisor_workflow::set_instance_in_user_state(
                                                user_state, instance,
                                            )
                                        {
                                            tracing::warn!(
                                                thread_id = %thread_id_owned,
                                                error = %err,
                                                "failed to merge workflow override before final persist"
                                            );
                                        }
                                    }
                                }
                                let supervisor_history = raw_history.clone();
                                let supervisor_todo_prompt = if active_supervisor {
                                    raw_user_state
                                        .as_ref()
                                        .and_then(|state| {
                                            todo::latest_unfinished_batch_system_prompt(state)
                                        })
                                        .or(initial_supervisor_todo_prompt.clone())
                                } else {
                                    None
                                };
                                for event in persist_turn(
                                    &self.memory, &thread_id_owned,
                                    raw_history.take(), raw_user_state.take(), skip_count, &tool_elapsed_ms,
                                ).await {
                                    yield event;
                                }
                                if let Some(steer) = steer_queue.as_ref() {
                                    if let Some(batch) = steer.drain_batch() {
                                        yield CatEvent::SteerInjected(crate::SteerInjectedEvent {
                                            steer_ids: batch.ids.clone(),
                                            session_id: thread_id_owned.clone(),
                                            preview: batch.preview.clone(),
                                            count: batch.count,
                                        });
                                        next_content = batch.content;
                                        continuation_from_supervisor = false;
                                        continuation_from_background_task = true;
                                        continue 'workflow_loop;
                                    }
                                }
                                if round_opts.async_agent {
                                    let background_task_count = self
                                        .tool_tasks
                                        .list(Some(&thread_id_owned))
                                        .await
                                        .into_iter()
                                        .filter(|task| {
                                            task.status == crate::tool_tasks::TOOL_TASK_RUNNING
                                        })
                                        .count();
                                    if background_task_count > 0 {
                                        yield CatEvent::BackgroundTasksWaiting {
                                            count: background_task_count,
                                        };
                                    }
                                    loop {
                                        while let Some(event) = try_recv_background_side_event(
                                            &mut background_side_events,
                                            &thread_id_owned,
                                        ) {
                                            yield event;
                                        }
                                        if let Some(task) = try_recv_completed_tool_task(
                                            &mut completed_tool_tasks,
                                            &thread_id_owned,
                                        ) {
                                            if !self
                                                .tool_tasks
                                                .claim_completion_notification(&task.task_id)
                                                .await
                                            {
                                                continue;
                                            }
                                            let fallback_content =
                                                background_task_completion_content(&task);
                                            if let Some(steer) = steer_queue.as_ref() {
                                                steer.push(background_task_completion_steer_input(&task));
                                            }
                                            yield CatEvent::ToolTaskCompleted(task);
                                            if let Some(steer) = steer_queue.as_ref() {
                                                if let Some(batch) = steer.drain_batch() {
                                                    yield CatEvent::SteerInjected(
                                                        crate::SteerInjectedEvent {
                                                            steer_ids: batch.ids.clone(),
                                                            session_id: thread_id_owned.clone(),
                                                            preview: batch.preview.clone(),
                                                            count: batch.count,
                                                        },
                                                    );
                                                    next_content = batch.content;
                                                    continuation_from_supervisor = false;
                                                    continuation_from_background_task = true;
                                                    continue 'workflow_loop;
                                                }
                                            } else {
                                                next_content = fallback_content;
                                                continuation_from_supervisor = false;
                                                continuation_from_background_task = true;
                                                continue 'workflow_loop;
                                            }
                                            continue;
                                        }
                                        while let Some(event) = try_recv_background_side_event(
                                            &mut background_side_events,
                                            &thread_id_owned,
                                        ) {
                                            yield event;
                                        }
                                        if let Some(steer) = steer_queue.as_ref() {
                                            if let Some(batch) = steer.drain_batch() {
                                                yield CatEvent::SteerInjected(
                                                    crate::SteerInjectedEvent {
                                                        steer_ids: batch.ids.clone(),
                                                        session_id: thread_id_owned.clone(),
                                                        preview: batch.preview.clone(),
                                                        count: batch.count,
                                                    },
                                                );
                                                next_content = batch.content;
                                                continuation_from_supervisor = false;
                                                continuation_from_background_task = true;
                                                continue 'workflow_loop;
                                            }
                                        }
                                        let pending_completion = self
                                            .tool_tasks
                                            .claim_pending_completion_notification(&thread_id_owned)
                                            .await;
                                        if let Some(task) = pending_completion {
                                            let fallback_content =
                                                background_task_completion_content(&task);
                                            if let Some(steer) = steer_queue.as_ref() {
                                                steer.push(background_task_completion_steer_input(&task));
                                            }
                                            yield CatEvent::ToolTaskCompleted(task);
                                            if let Some(steer) = steer_queue.as_ref() {
                                                if let Some(batch) = steer.drain_batch() {
                                                    yield CatEvent::SteerInjected(
                                                        crate::SteerInjectedEvent {
                                                            steer_ids: batch.ids.clone(),
                                                            session_id: thread_id_owned.clone(),
                                                            preview: batch.preview.clone(),
                                                            count: batch.count,
                                                        },
                                                    );
                                                    next_content = batch.content;
                                                    continuation_from_supervisor = false;
                                                    continuation_from_background_task = true;
                                                    continue 'workflow_loop;
                                                }
                                            } else {
                                                next_content = fallback_content;
                                                continuation_from_supervisor = false;
                                                continuation_from_background_task = true;
                                                continue 'workflow_loop;
                                            }
                                            continue;
                                        }
                                        if !self.tool_tasks.is_thread_running(&thread_id_owned).await {
                                            break;
                                        }
                                        // The foreground agent has already
                                        // finished at this point, so its inner
                                        // stream can no longer observe the
                                        // turn cancellation token. Include it
                                        // in this wait explicitly; otherwise a
                                        // Ctrl+C can leave the TUI stuck in
                                        // "cancelling" until a sub-task emits
                                        // another event or finishes.
                                        let cancel_wait = async {
                                            match round_opts.cancel.as_ref() {
                                                Some(cancel) => cancel.cancelled().await,
                                                None => std::future::pending::<()>().await,
                                            }
                                        };
                                        tokio::pin!(cancel_wait);
                                        let steer_wait = async {
                                            match steer_queue.as_ref() {
                                                Some(steer) => steer.notified().await,
                                                None => std::future::pending::<()>().await,
                                            }
                                        };
                                        tokio::pin!(steer_wait);
                                        tokio::select! {
                                            _ = &mut cancel_wait => {
                                                let cancelled_tasks = self
                                                    .cancel_background_tasks(&thread_id_owned)
                                                    .await;
                                                tracing::info!(
                                                    thread_id = %thread_id_owned,
                                                    cancelled_task_count = cancelled_tasks.len(),
                                                    "agent_turn.cancelled_while_waiting_for_background_tasks"
                                                );
                                                yield CatEvent::Cancelled;
                                                yield CatEvent::Done;
                                                return;
                                            }
                                            _ = &mut steer_wait => {}
                                            completed = completed_tool_tasks.recv() => {
                                                match completed {
                                                    Ok(task) if task.thread_id == thread_id_owned => {
                                                        if !self
                                                            .tool_tasks
                                                            .claim_completion_notification(&task.task_id)
                                                            .await
                                                        {
                                                            continue;
                                                        }
                                                        let fallback_content =
                                                            background_task_completion_content(&task);
                                                        if let Some(steer) = steer_queue.as_ref() {
                                                            steer.push(background_task_completion_steer_input(&task));
                                                        }
                                                        yield CatEvent::ToolTaskCompleted(task);
                                                        if let Some(steer) = steer_queue.as_ref() {
                                                            if let Some(batch) = steer.drain_batch() {
                                                                yield CatEvent::SteerInjected(
                                                                    crate::SteerInjectedEvent {
                                                                        steer_ids: batch.ids.clone(),
                                                                        session_id: thread_id_owned.clone(),
                                                                        preview: batch.preview.clone(),
                                                                        count: batch.count,
                                                                    },
                                                                );
                                                                next_content = batch.content;
                                                                continuation_from_supervisor = false;
                                                                continuation_from_background_task = true;
                                                                continue 'workflow_loop;
                                                            }
                                                        } else {
                                                            next_content = fallback_content;
                                                            continuation_from_supervisor = false;
                                                            continuation_from_background_task = true;
                                                            continue 'workflow_loop;
                                                        }
                                                    }
                                                    Ok(_) => {}
                                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                                }
                                            }
                                            side_event = background_side_events.recv() => {
                                                match side_event {
                                                    Ok((event_thread_id, event)) if event_thread_id == thread_id_owned => {
                                                        yield event;
                                                    }
                                                    Ok(_) => {}
                                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                                                }
                                            }
                                        }
                                    }
                                }
                                if !round_opts.supervisor_run
                                    && !user_question_requested_this_round
                                    && !workflow_interrupted_this_round
                                {
                                    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
                                    let evaluation = self.evaluate_workflow_after_round(
                                        &thread_id_owned,
                                        supervisor_history.as_deref().unwrap_or(&[]),
                                        supervisor_todo_prompt,
                                        supervisor_round,
                                        round_opts.model_profile_id.as_deref(),
                                        round_opts.cancel.clone(),
                                        progress_tx,
                                    );
                                    tokio::pin!(evaluation);
                                    let outcome = loop {
                                        tokio::select! {
                                            outcome = &mut evaluation => break outcome,
                                            progress = progress_rx.recv() => {
                                                if let Some(progress) = progress {
                                                    yield CatEvent::SupervisorProgress(progress);
                                                }
                                            }
                                        }
                                    };
                                    while let Ok(progress) = progress_rx.try_recv() {
                                        yield CatEvent::SupervisorProgress(progress);
                                    }
                                    match outcome {
                                        WorkflowRoundOutcome::Cancelled => {
                                            yield CatEvent::Cancelled;
                                            yield CatEvent::Done;
                                            return;
                                        }
                                        WorkflowRoundOutcome::NoWorkflow => {}
                                        WorkflowRoundOutcome::Report(report) => {
                                            yield CatEvent::Supervisor(report);
                                        }
                                        WorkflowRoundOutcome::Continue { report, message } => {
                                            yield CatEvent::Supervisor(report);
                                            supervisor_round = supervisor_round.saturating_add(1);
                                            next_content = Content::text(message);
                                            continuation_from_supervisor = true;
                                            continue 'workflow_loop;
                                        }
                                    }
                                }
                                tracing::info!(
                                    thread_id = %thread_id_owned,
                                    model_profile = %effective_model.profile.id,
                                    model = %effective_model.profile.model,
                                    supervisor_round,
                                    tool_calls = tool_elapsed_ms.len(),
                                    elapsed_ms = turn_started.elapsed().as_millis() as u64,
                                    "agent_turn.completed"
                                );
                                if !stop_hook_active {
                                    let last_assistant_message = supervisor_history
                                        .as_deref()
                                        .unwrap_or(&[])
                                        .iter()
                                        .rev()
                                        .find(|message| message.role == Role::Assistant)
                                        .map(|message| message.content.text_content())
                                        .unwrap_or_default();
                                    let stop_hook = self
                                        .hook_manager
                                        .run(
                                            HookEventName::Stop,
                                            None,
                                            &hook_context,
                                            serde_json::json!({
                                                "stop_hook_active": false,
                                                "last_assistant_message": last_assistant_message,
                                            }),
                                        )
                                        .await;
                                    if stop_hook.blocked || stop_hook.continue_flow == Some(false) {
                                        supervisor_round = supervisor_round.saturating_add(1);
                                        next_content = Content::text(stop_hook.reason.unwrap_or_else(|| {
                                            "Continue the response according to the Stop hook.".to_string()
                                        }));
                                        continuation_from_supervisor = true;
                                        stop_hook_active = true;
                                        continue 'workflow_loop;
                                    }
                                }
                                if let Some(steer) = steer_queue.as_ref() {
                                    if let Some(batch) = steer.drain_batch() {
                                        yield CatEvent::SteerInjected(crate::SteerInjectedEvent {
                                            steer_ids: batch.ids.clone(),
                                            session_id: thread_id_owned.clone(),
                                            preview: batch.preview.clone(),
                                            count: batch.count,
                                        });
                                        next_content = batch.content;
                                        continuation_from_supervisor = false;
                                        continuation_from_background_task = true;
                                        continue 'workflow_loop;
                                    }
                                }
                                yield CatEvent::Done;
                                return;
                            }
                            CatEvent::Error(e) => {
                                // Best-effort save on error (partial history is better than nothing).
                                for event in persist_turn(
                                    &self.memory, &thread_id_owned,
                                    raw_history.take(), raw_user_state.take(), skip_count, &tool_elapsed_ms,
                                ).await {
                                    yield event;
                                }
                                tracing::warn!(
                                    thread_id = %thread_id_owned,
                                    model_profile = %effective_model.profile.id,
                                    model = %effective_model.profile.model,
                                    supervisor_round,
                                    tool_calls = tool_elapsed_ms.len(),
                                    elapsed_ms = turn_started.elapsed().as_millis() as u64,
                                    error = %e,
                                    "agent_turn.failed"
                                );
                                yield CatEvent::Error(e);
                                return;
                            }
                            other => yield other,
                        }
                }

                // Fallback: stream ended without Done (shouldn't normally happen).
                for event in persist_turn(
                    &self.memory, &thread_id_owned,
                    raw_history.take(), raw_user_state.take(), skip_count, &tool_elapsed_ms,
                ).await {
                    yield event;
                }
                tracing::warn!(
                    thread_id = %thread_id_owned,
                    model_profile = %effective_model.profile.id,
                    model = %effective_model.profile.model,
                    supervisor_round,
                    tool_calls = tool_elapsed_ms.len(),
                    elapsed_ms = turn_started.elapsed().as_millis() as u64,
                    "agent_turn.failed"
                );
                break 'workflow_loop;
            }
        }
    }
}

// -- Helpers ------------------------------------------------------------------

fn summarize_content_for_log(content: &Content) -> String {
    match content {
        Content::Text(text) => format!("text(len={})", text.chars().count()),
        Content::Parts(parts) => {
            let mut text_len = 0usize;
            let mut image_urls: Vec<String> = Vec::new();
            let mut image_base64: Vec<String> = Vec::new();
            let mut audio_parts = 0usize;
            let mut file_parts = 0usize;

            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        text_len += text.chars().count();
                    }
                    ContentPart::ImageUrl { image_url } => {
                        image_urls.push(format!(
                            "{}(len={})",
                            preview_url_header(&image_url.url),
                            image_url.url.len()
                        ));
                    }
                    ContentPart::ImageBase64 { media_type, data } => {
                        image_base64.push(format!("{}(data_len={})", media_type, data.len()));
                    }
                    ContentPart::Audio { .. } => {
                        audio_parts += 1;
                    }
                    ContentPart::File { .. } => {
                        file_parts += 1;
                    }
                }
            }

            format!(
                "parts(total={}, text_len={}, image_urls={:?}, image_base64={:?}, audio_parts={}, file_parts={})",
                parts.len(),
                text_len,
                image_urls,
                image_base64,
                audio_parts,
                file_parts,
            )
        }
    }
}

fn preview_url_header(url: &str) -> &str {
    match url.find(',') {
        Some(idx) => &url[..idx],
        None => url,
    }
}

fn cat_agent_tool_definitions(
    agent: &CatAgent<InnerAgent>,
) -> Vec<remi_agentloop::tool::ToolDefinition> {
    use remi_agentloop::tool::registry::ToolRegistry;

    let mut definitions = agent.local_tools.definitions(&serde_json::Value::Null);
    if let Some(model_tools) = &agent.model_tools {
        definitions.extend(model_tools.definitions(&serde_json::Value::Null));
    }
    definitions
}

fn cat_agent_contains_tool(agent: &CatAgent<InnerAgent>, name: &str) -> bool {
    use remi_agentloop::tool::registry::ToolRegistry;

    agent.local_tools.contains(name)
        || agent
            .model_tools
            .as_ref()
            .map(|tools| tools.contains(name))
            .unwrap_or(false)
}

fn cat_agent_tool_available(agent: &CatAgent<InnerAgent>, name: &str) -> bool {
    cat_agent_contains_tool(agent, name)
        && agent
            .tool_allowlist
            .as_ref()
            .map(|allowlist| allowlist.iter().any(|tool| tool == name))
            .unwrap_or(true)
}

fn skill_prompt_tool_availability(agent: &CatAgent<InnerAgent>) -> SkillPromptToolAvailability {
    SkillPromptToolAvailability::new(
        cat_agent_tool_available(agent, "skill__search"),
        cat_agent_tool_available(agent, "skill__get"),
        cat_agent_tool_available(agent, "fs_read"),
    )
}

struct LocalToolDeps {
    skill_store: Arc<BuiltinSkillStore<FileSkillStore>>,
    memory: Arc<MemoryStore>,
    todo_backend: Arc<todo::HybridTodoBackend>,
    acp_backend: Arc<acp::AcpBackend>,
    sandbox: Arc<dyn sandbox::Sandbox>,
    environment_context_source: EnvironmentContextSource,
    bash_enabled: bool,
    redactor: SharedRedactor,
    data_dir: PathBuf,
    workspace_root: PathBuf,
    agents_dir: PathBuf,
    delegate_ids: Vec<String>,
    api_key: String,
    im_bridge: Option<Arc<dyn ImFileBridge>>,
    active_agent_id: String,
    approval_manager: Arc<ToolApprovalManager>,
    approval_reviewer: Option<Arc<ModelApprovalReviewer>>,
    user_question_manager: Arc<UserQuestionManager>,
    hook_manager: Arc<HookManager>,
    tool_tasks: Arc<crate::tool_tasks::ToolTaskManager>,
    overflow_bytes: usize,
    acp_client_tools: Option<(acp::AcpClientToolProvider, acp::AcpClientToolSupport)>,
}

impl LocalToolDeps {
    fn with_api_key(&self, api_key: String) -> Self {
        let mut deps = self.clone_for_subagent();
        deps.api_key = api_key;
        deps
    }

    fn clone_for_subagent(&self) -> Self {
        Self {
            skill_store: Arc::clone(&self.skill_store),
            memory: Arc::clone(&self.memory),
            todo_backend: Arc::clone(&self.todo_backend),
            acp_backend: Arc::clone(&self.acp_backend),
            sandbox: Arc::clone(&self.sandbox),
            environment_context_source: self.environment_context_source.clone(),
            bash_enabled: self.bash_enabled,
            redactor: Arc::clone(&self.redactor),
            data_dir: self.data_dir.clone(),
            workspace_root: self.workspace_root.clone(),
            agents_dir: self.agents_dir.clone(),
            delegate_ids: self.delegate_ids.clone(),
            api_key: self.api_key.clone(),
            im_bridge: self.im_bridge.clone(),
            active_agent_id: self.active_agent_id.clone(),
            approval_manager: Arc::clone(&self.approval_manager),
            approval_reviewer: self.approval_reviewer.clone(),
            user_question_manager: Arc::clone(&self.user_question_manager),
            hook_manager: Arc::clone(&self.hook_manager),
            tool_tasks: Arc::clone(&self.tool_tasks),
            overflow_bytes: self.overflow_bytes,
            acp_client_tools: self.acp_client_tools.clone(),
        }
    }

    fn build_static_tools(&self, include_acp: bool) -> DefaultToolRegistry {
        let mut local_tools = DefaultToolRegistry::new();
        skill::register_skill_tools(&mut local_tools, Arc::clone(&self.skill_store));
        todo::register_todo_tools(&mut local_tools, Arc::clone(&self.todo_backend));
        if include_acp {
            acp::register_acp_tools(
                &mut local_tools,
                Arc::clone(&self.acp_backend),
                Arc::clone(&self.approval_manager),
            );
        }
        register_runtime_tools(&mut local_tools, self, &self.active_agent_id, true);
        local_tools
    }

    fn build_model_tools(
        &self,
        profile: &ModelProfileConfig,
        extra_options: serde_json::Map<String, serde_json::Value>,
    ) -> DefaultToolRegistry {
        let mut local_tools = DefaultToolRegistry::new();
        register_delegate_agent_tools(
            &mut local_tools,
            self,
            &self.delegate_ids,
            self.api_key.clone(),
            profile.base_url.clone(),
            profile.model.clone(),
            extra_options,
            self.overflow_bytes,
        );
        local_tools
    }
}

// -- CatBotBuilder ------------------------------------------------------------

pub struct CatBotBuilder {
    api_key: String,
    model_profile: ModelProfileConfig,
    runtime_model_locked: bool,
    system: String,
    skills_dir: PathBuf,
    data_dir: PathBuf,
    /// If set, Agent.md is read from this path instead of `data_dir/Agent.md`.
    /// Allows placing Agent.md outside the agent's writable sandbox.
    agent_md_path: Option<PathBuf>,
    /// None → derive from model profile.
    overflow_bytes: Option<usize>,
    memory_days: u64,
    sandbox_config: SandboxConfig,
    im_bridge: Option<Arc<dyn ImFileBridge>>,
    extra_options: serde_json::Map<String, serde_json::Value>,
    tool_allowlist: Option<Vec<String>>,
    delegate_ids: Vec<String>,
    active_agent_id: String,
    model_bindings: AgentModelBindings,
    approval_model_profile_id: Option<String>,
    agents_dir: PathBuf,
    max_turns: Option<usize>,
    model_registry: Arc<ModelProfileRegistry>,
    acp_client_tools: Option<(acp::AcpClientToolProvider, acp::AcpClientToolSupport)>,
}

impl CatBotBuilder {
    pub fn from_env() -> anyhow::Result<Self> {
        let memory_days = std::env::var("REMI_MEMORY_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7_u64);
        let overflow_bytes = tool_output_overflow_bytes_from_env()?;
        let bash_mode = match std::env::var("REMI_SHELL_MODE")
            .or_else(|_| std::env::var("REMI_BASH_MODE"))
            .as_deref()
        {
            Ok("local") => BashMode::Local,
            _ => BashMode::Docker,
        };
        let agent_md_path = std::env::var("AGENT_MD_PATH").ok().map(PathBuf::from);
        let data_dir =
            PathBuf::from(std::env::var("REMI_DATA_DIR").unwrap_or_else(|_| ".remi-cat".into()));
        let sandbox_config = SandboxConfig::from_env(data_dir.clone(), bash_mode);
        let skills_dir = data_dir.join("skills");
        let models_dir = data_dir.join("models");
        install_embedded_model_profiles(&models_dir)?;
        let model_registry = Arc::new(ModelProfileRegistry::load(&models_dir)?);
        let resolved_model = resolve_model_profile_from_env(&models_dir)?;
        let api_key = api_key_from_env(&resolved_model.profile)?;
        let runtime_model_locked = std::env::var("REMI_MODEL_PROFILE")
            .ok()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let mut builder = Self {
            api_key,
            model_profile: resolved_model.profile,
            runtime_model_locked,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.clone(),
            agent_md_path,
            overflow_bytes,
            memory_days,
            sandbox_config,
            im_bridge: None,
            extra_options: resolved_model.extra_options,
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: std::env::var("REMI_APPROVAL_MODEL_PROFILE")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            agents_dir: data_dir.join("agents"),
            max_turns: None,
            model_registry,
            acp_client_tools: None,
        };
        let agent_id = std::env::var("REMI_AGENT_ID").unwrap_or_else(|_| DEFAULT_AGENT_ID.into());
        let agents_dir = std::env::var("REMI_AGENTS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("agents"));
        install_embedded_agent_profiles(&agents_dir)?;
        builder.agents_dir = agents_dir.clone();
        if let Ok(registry) = AgentRegistry::load(agents_dir) {
            if let Some(profile) = registry.get(&agent_id) {
                builder = builder.agent_profile(profile.clone())?;
            }
        }
        Ok(builder)
    }

    pub fn system(mut self, s: impl Into<String>) -> Self {
        self.system = s.into();
        self
    }

    pub fn skills_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.skills_dir = dir.into();
        self
    }

    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = dir.into();
        self
    }

    /// Override the tool-output overflow threshold (bytes).
    /// By default this is derived from the model profile.
    pub fn overflow_bytes(mut self, n: usize) -> Self {
        self.overflow_bytes = Some(n);
        self
    }

    pub fn im_bridge(mut self, bridge: Arc<dyn ImFileBridge>) -> Self {
        self.im_bridge = Some(bridge);
        self
    }

    pub fn approval_model_profile(mut self, profile_id: impl Into<String>) -> Self {
        let profile_id = profile_id.into();
        self.approval_model_profile_id = Some(profile_id);
        self
    }

    pub fn acp_client_tools(
        mut self,
        provider: acp::AcpClientToolProvider,
        support: acp::AcpClientToolSupport,
    ) -> Self {
        self.acp_client_tools = Some((provider, support));
        self
    }

    pub fn agent_profile(mut self, profile: AgentProfile) -> anyhow::Result<Self> {
        if !self.runtime_model_locked {
            if let Some(model_profile_id) = profile.models.primary.as_deref() {
                let resolved = self
                    .model_registry
                    .get(model_profile_id)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "agent `{}` references unknown model profile `{}`",
                            profile.id,
                            model_profile_id
                        )
                    })?;
                self.model_profile = resolved;
            }
            if let Some(model) = profile.model.filter(|value| !value.trim().is_empty()) {
                self.model_profile.model = model;
            }
            if let Some(base_url) = profile.base_url.filter(|value| !value.trim().is_empty()) {
                self.model_profile.base_url = Some(base_url);
            }
        }
        self.model_bindings = profile.models.clone();
        self.active_agent_id = profile.id.clone();
        self.system = profile.system_prompt;
        let mut tools = profile.tools;
        expand_skill_discovery_tools(&mut tools);
        for delegate in &profile.delegates {
            let name = delegate_tool_name(delegate);
            if !tools.iter().any(|tool| tool == &name) {
                tools.push(name);
            }
        }
        self.tool_allowlist = Some(tools);
        self.delegate_ids = profile.delegates;
        self.max_turns = profile.max_turns;
        Ok(self)
    }

    pub fn build(self) -> anyhow::Result<CatBot> {
        let profile = self.model_profile.clone();
        let auto_compress_context_percent = auto_compress_context_percent()?;
        let system_prompt = system_prompt_with_agent_md_notice_for_current_dir(self.system.clone());
        let short_term_tokens = memory_compaction_budget_tokens(
            &profile,
            auto_compress_context_percent,
            &system_prompt,
        );
        let overflow_bytes = self.overflow_bytes.unwrap_or(profile.overflow_bytes);
        let resolved_base_url = profile.base_url.clone();
        let approval_profile = if let Some(profile_id) = self.approval_model_profile_id.as_deref() {
            self.model_registry
                .get(profile_id)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("approval model profile `{profile_id}` was not found")
                })?
        } else {
            profile.clone()
        };
        let approval_api_key = api_key_from_env(&approval_profile)?;
        let approval_reviewer = Arc::new(ModelApprovalReviewer::new(
            approval_profile.clone(),
            approval_api_key,
        ));

        tracing::debug!(
            model = %profile.model,
            profile = %profile.id,
            approval_model_profile = %approval_profile.id,
            helper_model_profile = self.model_bindings.helper.as_deref().unwrap_or(""),
            vision_model_profile = self.model_bindings.vision.as_deref().unwrap_or(""),
            context_tokens = profile.context_tokens,
            max_output_tokens = profile.max_output_tokens,
            short_term_tokens,
            auto_compress_context_percent,
            overflow_bytes,
            base_url = ?resolved_base_url,
            "model profile resolved"
        );

        if !self.extra_options.is_empty() {
            tracing::info!(
                model = %profile.model,
                extra_options = ?self.extra_options,
                "model extra options enabled"
            );
        }

        let mut oai = OpenAIClient::new(self.api_key.clone()).with_model(profile.model.clone());
        if let Some(url) = resolved_base_url.clone() {
            oai = oai.with_base_url(url);
        }
        let extra_options = self.extra_options.clone();
        let agent_config = AgentConfig::default().with_max_tokens(profile.max_output_tokens);
        let mut inner_builder = AgentBuilder::new()
            .model(oai)
            .config(agent_config)
            .system(system_prompt.clone())
            .max_turns(self.max_turns.unwrap_or(usize::MAX));
        if !extra_options.is_empty() {
            inner_builder = inner_builder.extra_options(extra_options.clone());
        }

        // ── LangSmith tracing (optional) ──────────────────────────────────
        if let Ok(api_key) = std::env::var("LANGSMITH_API_KEY") {
            if !api_key.is_empty() {
                let project =
                    std::env::var("LANGSMITH_PROJECT").unwrap_or_else(|_| "remi-cat".into());
                tracing::info!(project = %project, "LangSmith tracing enabled");
                inner_builder = inner_builder.tracer(
                    remi_agentloop::prelude::LangSmithTracer::new(api_key).with_project(project),
                );
            }
        }

        let inner_loop: InnerAgent = inner_builder.build_loop();

        // Compressor uses the same API credentials but no tools, max 1 turn.
        let compressor = LlmCompressor::new(
            self.api_key.clone(),
            resolved_base_url.clone(),
            profile.model.clone(),
            profile.context_tokens,
            profile.max_output_tokens,
            extra_options,
        );

        let memory = Arc::new(MemoryStore {
            data_dir: self.data_dir,
            agent_md_path: self.agent_md_path,
            compressor,
            short_term_tokens,
            auto_compress: profile.auto_compress,
            memory_days: self.memory_days,
        });

        let workspace_root = self.sandbox_config.host_dir().to_path_buf();
        let environment_context_source =
            EnvironmentContextSource::from_sandbox_config(&self.sandbox_config);
        let skill_store = Arc::new(BuiltinSkillStore::new(
            FileSkillStore::new_in_workspace(self.skills_dir, workspace_root.clone()),
            [remi_skill::builtin_remi_skill()],
        ));
        let pinned_skill_summaries = skill_store
            .featured_summaries()
            .into_iter()
            .filter(|skill| skill.pin)
            .collect::<Vec<_>>();
        let data_dir = memory.data_dir.clone();
        let sandbox = self.sandbox_config.build()?;
        let agents_dir = self.agents_dir.clone();
        install_embedded_agent_profiles(&agents_dir)?;
        let active_agent_id = self.active_agent_id.clone();
        let todo_backend = Arc::new(todo::HybridTodoBackend::new(data_dir.clone()));
        let approval_manager = ToolApprovalManager::new();
        let user_question_manager = UserQuestionManager::new();
        let hook_manager = HookManager::new(workspace_root.clone(), data_dir.clone());
        let tool_tasks = crate::tool_tasks::ToolTaskManager::load(&data_dir)
            .map_err(|err| AgentError::other(format!("load tool task store: {err:#}")))?;
        let acp_backend = Arc::new(acp::AcpBackend::new(
            data_dir.clone(),
            self.im_bridge.clone(),
        ));
        let mut acp_local_model =
            OpenAIClient::new(self.api_key.clone()).with_model(profile.model.clone());
        if let Some(url) = resolved_base_url.clone() {
            acp_local_model = acp_local_model.with_base_url(url);
        }
        let mut acp_local_builder = AgentBuilder::new()
            .model(acp_local_model)
            .config(AgentConfig::default().with_max_tokens(profile.max_output_tokens))
            .system(system_prompt.clone())
            .max_turns(self.max_turns.unwrap_or(usize::MAX));
        if !self.extra_options.is_empty() {
            acp_local_builder = acp_local_builder.extra_options(self.extra_options.clone());
        }
        let acp_local_inner: InnerAgent = acp_local_builder.build_loop();
        let redactor: SharedRedactor = Arc::new(std::sync::RwLock::new(SecretRedactor::empty()));
        let acp_tool_deps = LocalToolDeps {
            skill_store: Arc::clone(&skill_store),
            memory: Arc::clone(&memory),
            todo_backend: Arc::clone(&todo_backend),
            acp_backend: Arc::clone(&acp_backend),
            sandbox: Arc::clone(&sandbox),
            environment_context_source: environment_context_source.clone(),
            bash_enabled: self.sandbox_config.bash_enabled(),
            redactor: Arc::clone(&redactor),
            data_dir: data_dir.clone(),
            workspace_root: workspace_root.clone(),
            agents_dir: agents_dir.clone(),
            delegate_ids: self.delegate_ids.clone(),
            api_key: self.api_key.clone(),
            im_bridge: self.im_bridge.clone(),
            active_agent_id: active_agent_id.clone(),
            approval_manager: Arc::clone(&approval_manager),
            approval_reviewer: Some(Arc::clone(&approval_reviewer)),
            user_question_manager: Arc::clone(&user_question_manager),
            hook_manager: Arc::clone(&hook_manager),
            tool_tasks: Arc::clone(&tool_tasks),
            overflow_bytes,
            acp_client_tools: self.acp_client_tools.clone(),
        };
        let mut acp_local_tools = DefaultToolRegistry::new();
        skill::register_skill_tools(&mut acp_local_tools, Arc::clone(&skill_store));
        todo::register_todo_tools(&mut acp_local_tools, Arc::clone(&todo_backend));
        register_delegate_agent_tools(
            &mut acp_local_tools,
            &acp_tool_deps,
            &self.delegate_ids,
            self.api_key.clone(),
            resolved_base_url.clone(),
            profile.model.clone(),
            self.extra_options.clone(),
            overflow_bytes,
        );
        register_runtime_tools(&mut acp_local_tools, &acp_tool_deps, &active_agent_id, true);
        let run_locks: ThreadRunLocks = Arc::new(AsyncMutex::new(HashMap::new()));
        let active_steers: ActiveSteerQueues = Arc::new(StdMutex::new(HashMap::new()));
        acp_backend.set_local_runner(Arc::new(LocalAcpAgentRunner {
            agent: CatAgent {
                inner: acp_local_inner,
                local_tools: Arc::new(acp_local_tools),
                model_tools: None,
                data_dir: memory.data_dir.clone(),
                workspace_root: workspace_root.clone(),
                workspace_root_label: sandbox.workspace_root_label(),
                allow_host_absolute_paths: sandbox.kind() != "docker",
                overflow_bytes,
                im_bridge: self.im_bridge.clone(),
                tool_allowlist: self.tool_allowlist.clone(),
                approval_manager: Arc::clone(&approval_manager),
                approval_reviewer: Some(Arc::clone(&approval_reviewer)),
                user_question_manager: Arc::clone(&user_question_manager),
                hook_manager: Arc::clone(&hook_manager),
                tool_tasks: Arc::clone(&tool_tasks),
            },
            memory: Arc::clone(&memory),
            run_locks: Arc::clone(&run_locks),
            system_prompt: system_prompt.clone(),
            environment_context_source: environment_context_source.clone(),
            model_profile: profile.clone(),
        }));
        let tool_deps = LocalToolDeps {
            skill_store: Arc::clone(&skill_store),
            memory: Arc::clone(&memory),
            todo_backend: Arc::clone(&todo_backend),
            acp_backend: Arc::clone(&acp_backend),
            sandbox: Arc::clone(&sandbox),
            environment_context_source: environment_context_source.clone(),
            bash_enabled: self.sandbox_config.bash_enabled(),
            redactor: Arc::clone(&redactor),
            data_dir: data_dir.clone(),
            workspace_root: workspace_root.clone(),
            agents_dir: agents_dir.clone(),
            delegate_ids: self.delegate_ids.clone(),
            api_key: self.api_key.clone(),
            im_bridge: self.im_bridge.clone(),
            active_agent_id: active_agent_id.clone(),
            approval_manager: Arc::clone(&approval_manager),
            approval_reviewer: Some(Arc::clone(&approval_reviewer)),
            user_question_manager: Arc::clone(&user_question_manager),
            hook_manager: Arc::clone(&hook_manager),
            tool_tasks: Arc::clone(&tool_tasks),
            overflow_bytes,
            acp_client_tools: self.acp_client_tools.clone(),
        };
        let local_tools = Arc::new(tool_deps.build_static_tools(true));
        let model_tools = tool_deps.build_model_tools(&profile, self.extra_options.clone());
        let mut model_agents = HashMap::new();
        for model_profile in self.model_registry.list() {
            for variant in model_runtime_variants(model_profile)? {
                if model_profile.id == profile.id && variant.key == model_profile.id {
                    continue;
                }
                let model_api_key = runtime_api_key_for_profile(&variant.profile);
                let model_inner = build_inner_agent(
                    &model_api_key,
                    &variant.profile,
                    system_prompt.clone(),
                    self.max_turns,
                    variant.extra_options.clone(),
                );
                let model_tools = tool_deps
                    .with_api_key(model_api_key)
                    .build_model_tools(&variant.profile, variant.extra_options);
                let model_overflow_bytes = self
                    .overflow_bytes
                    .unwrap_or(variant.profile.overflow_bytes);
                model_agents.insert(
                    variant.key,
                    CatAgent {
                        inner: model_inner,
                        local_tools: Arc::clone(&local_tools),
                        model_tools: Some(model_tools),
                        data_dir: memory.data_dir.clone(),
                        workspace_root: workspace_root.clone(),
                        workspace_root_label: sandbox.workspace_root_label(),
                        allow_host_absolute_paths: sandbox.kind() != "docker",
                        overflow_bytes: model_overflow_bytes,
                        im_bridge: self.im_bridge.clone(),
                        tool_allowlist: self.tool_allowlist.clone(),
                        approval_manager: Arc::clone(&approval_manager),
                        approval_reviewer: Some(Arc::clone(&approval_reviewer)),
                        user_question_manager: Arc::clone(&user_question_manager),
                        hook_manager: Arc::clone(&hook_manager),
                        tool_tasks: Arc::clone(&tool_tasks),
                    },
                );
            }
        }

        let agent_registry = AgentRegistry::load(&agents_dir)?;
        let mut agent_profiles = agent_registry
            .profiles()
            .cloned()
            .map(|profile| (profile.id.clone(), profile))
            .collect::<HashMap<_, _>>();
        let active_profile_from_builder = |existing: Option<&AgentProfile>| AgentProfile {
            id: active_agent_id.clone(),
            name: existing
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| active_agent_id.clone()),
            description: existing
                .map(|profile| profile.description.clone())
                .unwrap_or_else(|| "Runtime default agent".to_string()),
            model: existing.and_then(|profile| profile.model.clone()),
            base_url: existing.and_then(|profile| profile.base_url.clone()),
            models: self.model_bindings.clone(),
            tools: self.tool_allowlist.clone().unwrap_or_default(),
            delegates: self.delegate_ids.clone(),
            max_turns: self.max_turns,
            persistent_sessions: existing
                .map(|profile| profile.persistent_sessions)
                .unwrap_or(false),
            system_prompt: system_prompt.clone(),
        };
        if self.tool_allowlist.is_some() {
            let existing = agent_profiles.get(&active_agent_id).cloned();
            agent_profiles.insert(
                active_agent_id.clone(),
                active_profile_from_builder(existing.as_ref()),
            );
        } else {
            agent_profiles
                .entry(active_agent_id.clone())
                .or_insert_with(|| AgentProfile {
                    id: active_agent_id.clone(),
                    name: active_agent_id.clone(),
                    description: "Runtime default agent".to_string(),
                    model: None,
                    base_url: None,
                    models: self.model_bindings.clone(),
                    tools: Vec::new(),
                    delegates: Vec::new(),
                    max_turns: self.max_turns,
                    persistent_sessions: false,
                    system_prompt: system_prompt.clone(),
                });
        }
        for profile in agent_profiles.values_mut() {
            profile.system_prompt =
                system_prompt_with_agent_md_notice_for_current_dir(profile.system_prompt.clone());
        }
        let default_agent_profile = agent_profiles
            .get(&active_agent_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("active agent `{active_agent_id}` is unavailable"))?;
        let mut agent_runtimes = HashMap::<String, HashMap<String, CatAgent<InnerAgent>>>::new();
        for agent_profile in agent_profiles.values() {
            if agent_profile.id == active_agent_id {
                continue;
            }
            let agent_tool_deps = LocalToolDeps {
                skill_store: Arc::clone(&skill_store),
                memory: Arc::clone(&memory),
                todo_backend: Arc::clone(&todo_backend),
                acp_backend: Arc::clone(&acp_backend),
                sandbox: Arc::clone(&sandbox),
                environment_context_source: environment_context_source.clone(),
                bash_enabled: self.sandbox_config.bash_enabled(),
                redactor: Arc::clone(&redactor),
                data_dir: data_dir.clone(),
                workspace_root: workspace_root.clone(),
                agents_dir: agents_dir.clone(),
                delegate_ids: agent_profile.delegates.clone(),
                api_key: self.api_key.clone(),
                im_bridge: self.im_bridge.clone(),
                active_agent_id: agent_profile.id.clone(),
                approval_manager: Arc::clone(&approval_manager),
                approval_reviewer: Some(Arc::clone(&approval_reviewer)),
                user_question_manager: Arc::clone(&user_question_manager),
                hook_manager: Arc::clone(&hook_manager),
                tool_tasks: Arc::clone(&tool_tasks),
                overflow_bytes,
                acp_client_tools: self.acp_client_tools.clone(),
            };
            let agent_static_tools = Arc::new(agent_tool_deps.build_static_tools(true));
            let mut by_model = HashMap::new();
            for model_profile in self.model_registry.list() {
                for variant in model_runtime_variants(model_profile)? {
                    let model_api_key = runtime_api_key_for_profile(&variant.profile);
                    let model_inner = build_inner_agent(
                        &model_api_key,
                        &variant.profile,
                        agent_profile.system_prompt.clone(),
                        agent_profile.max_turns,
                        variant.extra_options.clone(),
                    );
                    let model_tools = agent_tool_deps
                        .with_api_key(model_api_key)
                        .build_model_tools(&variant.profile, variant.extra_options);
                    let agent_tool_allowlist = agent_tool_allowlist(agent_profile);
                    let model_overflow_bytes = self
                        .overflow_bytes
                        .unwrap_or(variant.profile.overflow_bytes);
                    by_model.insert(
                        variant.key,
                        CatAgent {
                            inner: model_inner,
                            local_tools: Arc::clone(&agent_static_tools),
                            model_tools: Some(model_tools),
                            data_dir: memory.data_dir.clone(),
                            workspace_root: workspace_root.clone(),
                            workspace_root_label: sandbox.workspace_root_label(),
                            allow_host_absolute_paths: sandbox.kind() != "docker",
                            overflow_bytes: model_overflow_bytes,
                            im_bridge: self.im_bridge.clone(),
                            tool_allowlist: Some(agent_tool_allowlist),
                            approval_manager: Arc::clone(&approval_manager),
                            approval_reviewer: Some(Arc::clone(&approval_reviewer)),
                            user_question_manager: Arc::clone(&user_question_manager),
                            hook_manager: Arc::clone(&hook_manager),
                            tool_tasks: Arc::clone(&tool_tasks),
                        },
                    );
                }
            }
            agent_runtimes.insert(agent_profile.id.clone(), by_model);
        }

        Ok(CatBot {
            inner: CatAgent {
                inner: inner_loop,
                local_tools,
                model_tools: Some(model_tools),
                data_dir: memory.data_dir.clone(),
                workspace_root: workspace_root.clone(),
                workspace_root_label: sandbox.workspace_root_label(),
                allow_host_absolute_paths: sandbox.kind() != "docker",
                overflow_bytes,
                im_bridge: self.im_bridge,
                tool_allowlist: self.tool_allowlist,
                approval_manager: Arc::clone(&approval_manager),
                approval_reviewer: Some(Arc::clone(&approval_reviewer)),
                user_question_manager: Arc::clone(&user_question_manager),
                hook_manager: Arc::clone(&hook_manager),
                tool_tasks: Arc::clone(&tool_tasks),
            },
            model_agents,
            agent_runtimes,
            agent_profiles,
            default_agent_profile,
            skill_store,
            pinned_skill_summaries,
            memory,
            todo_backend,
            acp_backend,
            approval_manager,
            user_question_manager,
            hook_manager,
            tool_tasks,
            run_locks,
            active_steers,
            model_profile: profile,
            model_registry: Arc::clone(&self.model_registry),
            environment_context_source,
            redactor,
        })
    }
}

fn runtime_api_key_for_profile(profile: &ModelProfileConfig) -> String {
    match api_key_from_env(profile) {
        Ok(api_key) => api_key,
        Err(err) => {
            tracing::warn!(
                model_profile = %profile.id,
                provider = profile.provider.as_deref().unwrap_or(""),
                model = %profile.model,
                error = %err,
                "model profile API key is unavailable; requests for this profile will fail until configured"
            );
            String::new()
        }
    }
}

struct ModelRuntimeVariant {
    key: String,
    profile: ModelProfileConfig,
    extra_options: serde_json::Map<String, serde_json::Value>,
}

fn model_runtime_key(profile_id: &str, reasoning_effort: Option<ReasoningEffort>) -> String {
    match reasoning_effort {
        Some(effort) if effort != ReasoningEffort::Auto => {
            format!("{profile_id}::reasoning={}", effort.as_str())
        }
        _ => profile_id.to_string(),
    }
}

fn model_runtime_variants(
    profile: &ModelProfileConfig,
) -> anyhow::Result<Vec<ModelRuntimeVariant>> {
    let mut variants = vec![ModelRuntimeVariant {
        key: profile.id.clone(),
        profile: profile.clone(),
        extra_options: profile.merged_extra_options(None, None)?,
    }];

    for effort in ReasoningEffort::VARIANTS {
        if effort == ReasoningEffort::Auto {
            continue;
        }
        let extra_options = match profile.merged_extra_options(None, Some(effort)) {
            Ok(options) => options,
            Err(err) => {
                tracing::debug!(
                    model_profile = %profile.id,
                    model = %profile.model,
                    reasoning_effort = effort.as_str(),
                    error = %err,
                    "skipping unsupported reasoning runtime variant"
                );
                continue;
            }
        };
        let mut variant_profile = profile.clone();
        variant_profile.reasoning_effort = Some(effort);
        variants.push(ModelRuntimeVariant {
            key: model_runtime_key(&profile.id, Some(effort)),
            profile: variant_profile,
            extra_options,
        });
    }

    Ok(variants)
}

fn apply_reasoning_effort_override(
    mut effective: EffectiveModelProfile,
    reasoning_effort: Option<ReasoningEffort>,
) -> anyhow::Result<EffectiveModelProfile> {
    let Some(effort) = reasoning_effort else {
        return Ok(effective);
    };
    if effort == ReasoningEffort::Auto {
        return Ok(effective);
    }
    effective
        .profile
        .merged_extra_options(None, Some(effort))
        .map(|_| ())?;
    effective.profile.reasoning_effort = Some(effort);
    Ok(effective)
}

fn delegate_tool_name(agent_id: &str) -> String {
    format!("agent__{}", agent_id.trim().replace('-', "_"))
}

fn agent_tool_allowlist(profile: &AgentProfile) -> Vec<String> {
    let mut tools = profile.tools.clone();
    expand_skill_discovery_tools(&mut tools);
    for delegate in &profile.delegates {
        let name = delegate_tool_name(delegate);
        if !tools.iter().any(|tool| tool == &name) {
            tools.push(name);
        }
    }
    tools
}

fn expand_skill_discovery_tools(tools: &mut Vec<String>) {
    if tools.iter().any(|tool| tool == "search")
        && tools.iter().any(|tool| tool == "skill__get")
        && !tools.iter().any(|tool| tool == "skill__search")
    {
        tools.push("skill__search".to_string());
    }
}

const SUBAGENT_APPROVAL_MARKER_AGENT: &str = "__remi_tool_approval__";
const SUBAGENT_APPROVAL_REQUESTED_PREFIX: &str = "__remi_tool_approval_requested__:";
const SUBAGENT_APPROVAL_UPDATED_PREFIX: &str = "__remi_tool_approval_updated__:";
const SUBAGENT_APPROVAL_RESOLVED_PREFIX: &str = "__remi_tool_approval_resolved__:";
const USER_QUESTION_REQUESTED_PREFIX: &str = "__remi_user_question_requested__:";
const USER_QUESTION_UPDATED_PREFIX: &str = "__remi_user_question_updated__:";
const USER_QUESTION_RESOLVED_PREFIX: &str = "__remi_user_question_resolved__:";

struct RemiSubAgentTool {
    name: String,
    description: String,
    parameters_schema: serde_json::Value,
    agent_name: String,
    model_name: String,
    base_url: Option<String>,
    api_key: String,
    system_prompt: String,
    extra_options: serde_json::Map<String, serde_json::Value>,
    max_turns: usize,
    data_dir: PathBuf,
    deps: LocalToolDeps,
    profile: AgentProfile,
    tool_allowlist: Vec<String>,
    overflow_bytes: usize,
    persistent_sessions: bool,
    session_locks: Arc<AsyncMutex<HashMap<String, ThreadRunLock>>>,
}

impl RemiSubAgentTool {
    fn title_from_args(arguments: &serde_json::Value) -> Option<String> {
        arguments
            .get("task")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
    }

    fn named_from_args(arguments: &serde_json::Value) -> Result<Option<String>, AgentError> {
        let Some(named) = arguments
            .get("named")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        if named.len() > 64 {
            return Err(AgentError::tool(
                "sub-agent",
                "named sub-agent session must be at most 64 characters",
            ));
        }
        if !named
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(AgentError::tool(
                "sub-agent",
                "named sub-agent session may only contain ASCII letters, numbers, '-' and '_'",
            ));
        }
        Ok(Some(named.to_string()))
    }

    async fn session_lock(&self, thread_id: &str) -> ThreadRunLock {
        let mut locks = self.session_locks.lock().await;
        locks
            .entry(thread_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

impl Tool for RemiSubAgentTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.parameters_schema.clone()
    }

    #[allow(refining_impl_trait)]
    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: ToolContext,
    ) -> Result<ToolResult<remi_agentloop::tool::BoxedToolStream>, AgentError> {
        let task = arguments
            .get("task")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if task.trim().is_empty() {
            return Err(AgentError::tool("sub-agent", "missing task"));
        }
        let named = Self::named_from_args(&arguments)?;
        if named.is_some() && !self.persistent_sessions {
            return Err(AgentError::tool(
                "sub-agent",
                "named sub-agent sessions are not enabled for this agent profile",
            ));
        }

        let title = Self::title_from_args(&arguments);
        let agent_name = self.agent_name.clone();
        let mut model = OpenAIClient::new(self.api_key.clone()).with_model(self.model_name.clone());
        if let Some(url) = self.base_url.clone() {
            model = model.with_base_url(url);
        }
        let mut builder = AgentBuilder::new()
            .model(model)
            .system(self.system_prompt.clone())
            .max_turns(self.max_turns);
        if !self.extra_options.is_empty() {
            builder = builder.extra_options(self.extra_options.clone());
        }
        let agent = CatAgent {
            inner: builder.build_loop(),
            local_tools: Arc::new(build_subagent_tools(&self.deps, &self.profile)),
            model_tools: None,
            data_dir: self.data_dir.clone(),
            workspace_root: self.deps.workspace_root.clone(),
            workspace_root_label: self.deps.sandbox.workspace_root_label(),
            allow_host_absolute_paths: self.deps.sandbox.kind() != "docker",
            overflow_bytes: self.overflow_bytes,
            im_bridge: None,
            tool_allowlist: Some(self.tool_allowlist.clone()),
            approval_manager: Arc::clone(&self.deps.approval_manager),
            approval_reviewer: self.deps.approval_reviewer.clone(),
            user_question_manager: Arc::clone(&self.deps.user_question_manager),
            hook_manager: Arc::clone(&self.deps.hook_manager),
            tool_tasks: Arc::clone(&self.deps.tool_tasks),
        };
        let sub_thread_id = ThreadId(match named.as_deref() {
            Some(named) => format!("subagent:{}:{named}", self.agent_name),
            None => format!("subagent:{}", uuid::Uuid::new_v4()),
        });
        let sub_run_id = RunId(uuid::Uuid::new_v4().to_string());
        let sub_thread_id_for_memory = sub_thread_id.0.clone();
        let metadata = subagent_metadata(ctx, &sub_thread_id_for_memory);
        let memory = Arc::clone(&self.deps.memory);
        let workspace_root = self.deps.workspace_root.clone();
        let environment_context_source = self.deps.environment_context_source.clone();
        let subagent_system_prompt = self.system_prompt.clone();
        let model_name = self.model_name.clone();
        let hook_manager = Arc::clone(&self.deps.hook_manager);
        let persistent = named.is_some();
        let session_lock = if persistent {
            Some(self.session_lock(&sub_thread_id_for_memory).await)
        } else {
            None
        };

        let output: remi_agentloop::tool::BoxedToolStream = Box::pin(stream! {
            let _session_guard = match session_lock.as_ref() {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            let mut sub_task = task.clone();
            let sub_hook_context = HookContext {
                session_id: sub_thread_id_for_memory.clone(),
                transcript_path: None,
                cwd: workspace_root.clone(),
                model: Some(model_name.clone()),
                turn_id: Some(sub_run_id.0.clone()),
                permission_mode: None,
            };
            let start_hook = hook_manager
                .run(
                    HookEventName::SubagentStart,
                    Some(&agent_name),
                    &sub_hook_context,
                    serde_json::json!({
                        "agent_id": &sub_thread_id_for_memory,
                        "agent_type": &agent_name,
                        "agent_transcript_path": null,
                    }),
                )
                .await;
            if start_hook.blocked || start_hook.continue_flow == Some(false) {
                tracing::warn!(
                    agent = %agent_name,
                    reason = start_hook.reason.as_deref().unwrap_or(""),
                    "SubagentStart hook requested stop; Codex compatibility ignores this decision"
                );
            }
            for context in start_hook.additional_context {
                sub_task.push_str("\n\n");
                sub_task.push_str(&format!("Hook SubagentStart provided additional context:\n{context}"));
            }
            yield ToolOutput::SubSession(SubSessionEvent::new(
                String::new(),
                sub_thread_id.clone(),
                sub_run_id.clone(),
                agent_name.clone(),
                title.clone(),
                0,
                ProtocolEvent::RunStart {
                    thread_id: sub_thread_id.to_string(),
                    run_id: sub_run_id.to_string(),
                    metadata: None,
                },
            ));

            let mut final_output = String::new();
            let mut ephemeral_user_state = serde_json::Value::Null;
            let ephemeral_environment_context = ensure_environment_context(
                &mut ephemeral_user_state,
                &environment_context_source,
            );
            let mut ephemeral_history = vec![Message::system(subagent_system_prompt.clone())];
            insert_environment_context_prompt(
                &mut ephemeral_history,
                1,
                ephemeral_environment_context.prompt,
            );
            let mut skip_count = ephemeral_history.len();
            let mut input = LoopInput::start(&sub_task)
                .history(ephemeral_history)
                .metadata(metadata.clone())
                .user_state(ephemeral_user_state);
            if persistent {
                match memory.load_context(&sub_thread_id_for_memory).await {
                    Ok(mut ctx) => {
                        let environment_context = ensure_environment_context(
                            &mut ctx.user_state,
                            &environment_context_source,
                        );
                        persist_new_environment_context(
                            &memory,
                            &sub_thread_id_for_memory,
                            &ctx.user_state,
                            environment_context.initialized,
                        )
                        .await;
                        let mut history = build_injected_history(&ctx);
                        history.insert(0, Message::system(subagent_system_prompt.clone()));
                        insert_environment_context_prompt(
                            &mut history,
                            1,
                            environment_context.prompt,
                        );
                        skip_count = history.len();
                        input = input
                            .history(history)
                            .user_state(std::mem::take(&mut ctx.user_state));
                    }
                    Err(err) => {
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            String::new(),
                            sub_thread_id.clone(),
                            sub_run_id.clone(),
                            agent_name.clone(),
                            title.clone(),
                            0,
                            ProtocolEvent::Error {
                                message: format!("failed to load sub-agent session: {err}"),
                                code: None,
                            },
                        ));
                        yield ToolOutput::text(format_subagent_tool_result(
                            &sub_thread_id,
                            &sub_run_id,
                            &agent_name,
                            false,
                            "",
                            Some(&err.to_string()),
                        ));
                        return;
                    }
                }
            }
            let mut raw_history: Option<Vec<Message>> = None;
            let mut raw_user_state: Option<serde_json::Value> = None;
            let mut subagent_stop_hook_active = false;
            let mut inner_stream = Box::pin(agent.stream_with_input(input));
            while let Some(event) = inner_stream.next().await {
                match event {
                    CatEvent::History(messages, user_state) => {
                        if persistent {
                            let next_skip_count = messages.len();
                            let _ = persist_turn(
                                &memory,
                                &sub_thread_id_for_memory,
                                Some(messages),
                                Some(user_state),
                                skip_count,
                                &HashMap::new(),
                            )
                            .await;
                            skip_count = next_skip_count;
                        } else {
                            raw_history = Some(messages);
                            raw_user_state = Some(user_state);
                        }
                    }
                    CatEvent::StateUpdate(user_state) => {
                        if persistent {
                            let _ = persist_turn(
                                &memory,
                                &sub_thread_id_for_memory,
                                None,
                                Some(user_state),
                                skip_count,
                                &HashMap::new(),
                            )
                            .await;
                        } else {
                            raw_user_state = Some(user_state);
                        }
                    }
                    CatEvent::Text(content) => {
                        final_output.push_str(&content);
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            String::new(),
                            sub_thread_id.clone(),
                            sub_run_id.clone(),
                            agent_name.clone(),
                            title.clone(),
                            0,
                            ProtocolEvent::Delta {
                                content,
                                role: None,
                            },
                        ));
                    }
                    CatEvent::Thinking(content) => {
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            String::new(),
                            sub_thread_id.clone(),
                            sub_run_id.clone(),
                            agent_name.clone(),
                            title.clone(),
                            0,
                            ProtocolEvent::ThinkingEnd { content },
                        ));
                    }
                    CatEvent::ToolCallStart { id, name } | CatEvent::ToolCall { id, name, .. } => {
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            String::new(),
                            sub_thread_id.clone(),
                            sub_run_id.clone(),
                            agent_name.clone(),
                            title.clone(),
                            0,
                            ProtocolEvent::ToolCallStart { id, name },
                        ));
                    }
                    CatEvent::ToolCallArgumentsDelta { id, delta } => {
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            String::new(),
                            sub_thread_id.clone(),
                            sub_run_id.clone(),
                            agent_name.clone(),
                            title.clone(),
                            0,
                            ProtocolEvent::ToolCallDelta {
                                id,
                                arguments_delta: delta,
                            },
                        ));
                    }
                    CatEvent::ToolCallResult { id, name, result, .. } => {
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            String::new(),
                            sub_thread_id.clone(),
                            sub_run_id.clone(),
                            agent_name.clone(),
                            title.clone(),
                            0,
                            ProtocolEvent::ToolResult { id, name, result },
                        ));
                    }
                    CatEvent::ToolApprovalRequested(request) => {
                        yield subagent_approval_marker(
                            &sub_thread_id,
                            &sub_run_id,
                            SUBAGENT_APPROVAL_REQUESTED_PREFIX,
                            serde_json::to_string(&request).unwrap_or_default(),
                        );
                    }
                    CatEvent::ToolApprovalUpdated(request) => {
                        yield subagent_approval_marker(
                            &sub_thread_id,
                            &sub_run_id,
                            SUBAGENT_APPROVAL_UPDATED_PREFIX,
                            serde_json::to_string(&request).unwrap_or_default(),
                        );
                    }
                    CatEvent::ToolApprovalResolved { request, decision } => {
                        yield subagent_approval_marker(
                            &sub_thread_id,
                            &sub_run_id,
                            SUBAGENT_APPROVAL_RESOLVED_PREFIX,
                            serde_json::to_string(&serde_json::json!({
                                "request": request,
                                "decision": decision,
                            }))
                            .unwrap_or_default(),
                        );
                    }
                    CatEvent::UserQuestionRequested(request) => {
                        yield subagent_approval_marker(
                            &sub_thread_id,
                            &sub_run_id,
                            USER_QUESTION_REQUESTED_PREFIX,
                            serde_json::to_string(&request).unwrap_or_default(),
                        );
                    }
                    CatEvent::UserQuestionUpdated(request) => {
                        yield subagent_approval_marker(
                            &sub_thread_id,
                            &sub_run_id,
                            USER_QUESTION_UPDATED_PREFIX,
                            serde_json::to_string(&request).unwrap_or_default(),
                        );
                    }
                    CatEvent::UserQuestionResolved { request, response } => {
                        yield subagent_approval_marker(
                            &sub_thread_id,
                            &sub_run_id,
                            USER_QUESTION_RESOLVED_PREFIX,
                            serde_json::to_string(&serde_json::json!({
                                "request": request,
                                "response": response,
                            }))
                            .unwrap_or_default(),
                        );
                    }
                    CatEvent::Done => {
                        if persistent {
                            let _ = persist_turn(
                                &memory,
                                &sub_thread_id_for_memory,
                                raw_history.take(),
                                raw_user_state.take(),
                                skip_count,
                                &HashMap::new(),
                            )
                            .await;
                        }
                        let stop_hook = hook_manager
                            .run(
                                HookEventName::SubagentStop,
                                Some(&agent_name),
                                &sub_hook_context,
                                serde_json::json!({
                                    "agent_id": &sub_thread_id_for_memory,
                                    "agent_type": &agent_name,
                                    "agent_transcript_path": null,
                                    "stop_hook_active": subagent_stop_hook_active,
                                    "last_assistant_message": &final_output,
                                }),
                            )
                            .await;
                        if (stop_hook.blocked || stop_hook.continue_flow == Some(false))
                            && !subagent_stop_hook_active
                        {
                            let message = stop_hook
                                .reason
                                .unwrap_or_else(|| "Run one more focused pass inside the subagent.".to_string());
                            subagent_stop_hook_active = true;
                            let mut continuation_input =
                                LoopInput::start(&message).metadata(metadata.clone());
                            if persistent {
                                if let Ok(mut ctx) = memory.load_context(&sub_thread_id_for_memory).await {
                                    let environment_context = ensure_environment_context(
                                        &mut ctx.user_state,
                                        &environment_context_source,
                                    );
                                    let mut history = build_injected_history(&ctx);
                                    history.insert(0, Message::system(subagent_system_prompt.clone()));
                                    insert_environment_context_prompt(
                                        &mut history,
                                        1,
                                        environment_context.prompt,
                                    );
                                    skip_count = history.len();
                                    continuation_input = continuation_input
                                        .history(history)
                                        .user_state(std::mem::take(&mut ctx.user_state));
                                }
                            } else if let Some(history) = raw_history.take() {
                                skip_count = history.len();
                                continuation_input = continuation_input.history(history);
                            }
                            raw_user_state = None;
                            inner_stream = Box::pin(agent.stream_with_input(continuation_input));
                            continue;
                        }
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            String::new(),
                            sub_thread_id.clone(),
                            sub_run_id.clone(),
                            agent_name.clone(),
                            title.clone(),
                            0,
                            sub_session_done_event(if final_output.trim().is_empty() {
                                None
                            } else {
                                Some(final_output.clone())
                            }),
                        ));
                        yield ToolOutput::text(format_subagent_tool_result(
                            &sub_thread_id,
                            &sub_run_id,
                            &agent_name,
                            true,
                            &final_output,
                            None,
                        ));
                        return;
                    }
                    CatEvent::Error(error) => {
                        let message = error.to_string();
                        if persistent {
                            let _ = persist_turn(
                                &memory,
                                &sub_thread_id_for_memory,
                                raw_history.take(),
                                raw_user_state.take(),
                                skip_count,
                                &HashMap::new(),
                            )
                            .await;
                        }
                        yield ToolOutput::SubSession(SubSessionEvent::new(
                            String::new(),
                            sub_thread_id.clone(),
                            sub_run_id.clone(),
                            agent_name.clone(),
                            title.clone(),
                            0,
                            ProtocolEvent::Error {
                                message: message.clone(),
                                code: None,
                            },
                        ));
                        yield ToolOutput::text(format_subagent_tool_result(
                            &sub_thread_id,
                            &sub_run_id,
                            &agent_name,
                            false,
                            "",
                            Some(&message),
                        ));
                        return;
                    }
                    _ => {}
                }
            }
            yield ToolOutput::SubSession(SubSessionEvent::new(
                String::new(),
                sub_thread_id.clone(),
                sub_run_id.clone(),
                agent_name.clone(),
                title.clone(),
                0,
                sub_session_done_event(if final_output.trim().is_empty() {
                    None
                } else {
                    Some(final_output.clone())
                }),
            ));
            yield ToolOutput::text(format_subagent_tool_result(
                &sub_thread_id,
                &sub_run_id,
                &agent_name,
                true,
                &final_output,
                None,
            ));
        });
        Ok(ToolResult::Output(output))
    }
}

fn format_subagent_tool_result(
    sub_thread_id: &ThreadId,
    sub_run_id: &RunId,
    agent_name: &str,
    success: bool,
    final_output: &str,
    error: Option<&str>,
) -> String {
    let value = serde_json::json!({
        "sub_session_id": &sub_thread_id.0,
        "sub_run_id": &sub_run_id.0,
        "agent": agent_name,
        "success": success,
        "final_output": final_output,
        "error": error,
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| final_output.to_string())
}

fn sub_session_done_event(final_output: Option<String>) -> ProtocolEvent {
    match final_output {
        Some(final_output) => ProtocolEvent::Custom {
            event_type: "sub_session_done".to_string(),
            extra: serde_json::json!({ "final_output": final_output }),
        },
        None => ProtocolEvent::Done,
    }
}

fn subagent_metadata(ctx: ToolContext, sub_thread_id: &str) -> serde_json::Value {
    let mut metadata = ctx.metadata().unwrap_or_else(|| serde_json::json!({}));
    if !metadata.is_object() {
        metadata = serde_json::json!({});
    }
    if let serde_json::Value::Object(map) = &mut metadata {
        map.insert(
            "thread_id".to_string(),
            serde_json::Value::String(sub_thread_id.to_string()),
        );
        if !map.contains_key("parent_thread_id") {
            map.insert(
                "parent_thread_id".to_string(),
                serde_json::Value::String(ctx.thread_id().0),
            );
        }
        map.insert(
            "subagent_run".to_string(),
            serde_json::Value::String("true".to_string()),
        );
    }
    metadata
}

fn register_delegate_agent_tools(
    registry: &mut DefaultToolRegistry,
    deps: &LocalToolDeps,
    delegate_ids: &[String],
    api_key: String,
    base_url: Option<String>,
    default_model: String,
    extra_options: serde_json::Map<String, serde_json::Value>,
    overflow_bytes: usize,
) {
    let agent_registry = match AgentRegistry::load(&deps.agents_dir) {
        Ok(registry) => Some(registry),
        Err(err) => {
            tracing::warn!(
                agents_dir = %deps.agents_dir.display(),
                error = %err,
                "agent profile registry could not be loaded; using embedded delegate fallbacks"
            );
            None
        }
    };
    for delegate_id in delegate_ids {
        let profile = match agent_registry
            .as_ref()
            .and_then(|registry| registry.get(delegate_id))
            .cloned()
        {
            Some(profile) => profile,
            None => match embedded_agent_profile(delegate_id) {
                Ok(Some(profile)) => profile,
                Ok(None) => {
                    tracing::warn!(delegate_id, "delegate agent profile not found");
                    continue;
                }
                Err(err) => {
                    tracing::warn!(
                        delegate_id,
                        error = %err,
                        "embedded delegate agent profile is invalid"
                    );
                    continue;
                }
            },
        };
        let tool_name = delegate_tool_name(&profile.id);
        let tool_description = format!(
            "Delegate a focused task to the `{}` sub-agent. {}",
            profile.name, profile.description
        );
        let agent_name = profile.id.clone();
        let system_prompt =
            system_prompt_with_agent_md_notice_for_current_dir(profile.system_prompt.clone());
        let model_name = profile
            .model
            .clone()
            .unwrap_or_else(|| default_model.clone());
        let tool_base_url = profile.base_url.clone().or_else(|| base_url.clone());
        let max_turns = profile.max_turns.unwrap_or(12);
        let tool_api_key = api_key.clone();
        let tool_extra_options = extra_options.clone();
        let persistent_sessions = profile.persistent_sessions;
        let mut tool_allowlist = profile.tools.clone();
        expand_skill_discovery_tools(&mut tool_allowlist);
        for delegate in &profile.delegates {
            let name = delegate_tool_name(delegate);
            if !tool_allowlist.iter().any(|tool| tool == &name) {
                tool_allowlist.push(name);
            }
        }
        let mut parameters_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Complete, self-contained task for the sub-agent. Include all necessary context."
                }
            },
            "required": ["task"]
        });
        if profile.persistent_sessions {
            if let Some(properties) = parameters_schema
                .get_mut("properties")
                .and_then(serde_json::Value::as_object_mut)
            {
                properties.insert(
                    "named".to_string(),
                    serde_json::json!({
                        "type": "string",
                        "description": "Optional named persistent sub-agent session. Calls with the same name reuse the same sub-agent conversation and memory."
                    }),
                );
            }
        }
        registry.register(RemiSubAgentTool {
            name: tool_name,
            description: tool_description,
            parameters_schema,
            agent_name,
            model_name,
            base_url: tool_base_url,
            api_key: tool_api_key,
            system_prompt,
            extra_options: tool_extra_options,
            max_turns,
            data_dir: deps.data_dir.clone(),
            deps: deps.clone_for_subagent(),
            profile,
            tool_allowlist,
            overflow_bytes,
            persistent_sessions,
            session_locks: Arc::new(AsyncMutex::new(HashMap::new())),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        append_thread_todo_system_prompt, background_task_completion_steer_input,
        context_percent_tokens, default_system_prompt, format_subagent_tool_result,
        insert_async_tool_system_prompt, insert_single_chat_sender_system_prompt,
        install_embedded_model_profiles, local_acp_thread_id, memory_compaction_budget_tokens,
        model_input_snapshot_from_loop_input, model_request_budget_tokens,
        prepend_group_sender_username, route_thread_todo_prompt, single_chat_sender_system_prompt,
        system_prompt_with_agent_md_notice, thread_run_lock, try_recv_background_side_event,
        try_recv_completed_tool_task, AgentModelBindings, CatBotBuilder, CatEvent, Content,
        ContentPart, GoalMaxRounds, LlmCompressor, LoopInput, Message, ModelProfileRegistry,
        PartialTurnRecorder, RemiSubAgentTool, SandboxConfig, StreamOptions, SupervisorTraceEvent,
        ThreadRunLocks, WorkflowStatus, AGENT_MD_CWD_SYSTEM_PROMPT_NOTICE,
        ASYNC_TOOL_SYSTEM_PROMPT, DEFAULT_AGENT_ID, DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT,
    };
    use crate::memory::{build_injected_history, MemoryContext, MemoryIndex};
    use crate::model_profile::ModelProfileConfig;
    use crate::supervisor_workflow;
    use crate::todo::tools::TodoItem;
    use crate::user_question::{UserQuestionResponse, UserQuestionStatus};
    use crate::{estimate_model_input_tokens, ModelInputSegmentCategory};
    use futures::StreamExt as _;
    use remi_agentloop::prelude::CancellationToken;
    use remi_agentloop::prelude::Role;
    use remi_agentloop::types::{FunctionCall, RunId, ThreadId, ToolCallMessage};
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex, OnceLock};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;
    use uuid::Uuid;

    static LARGE_STACK_TEST_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();

    fn run_large_stack_local_test<F, Fut>(test: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        std::thread::Builder::new()
            .name("bot-core-local-test".to_string())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let _guard = LARGE_STACK_TEST_LOCK
                    .get_or_init(|| StdMutex::new(()))
                    .lock()
                    .expect("large-stack test lock poisoned");
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build test runtime");
                let local = tokio::task::LocalSet::new();
                runtime.block_on(local.run_until(test()));
            })
            .expect("spawn large-stack test thread")
            .join()
            .expect("large-stack test thread panicked");
    }

    fn test_model_profile() -> ModelProfileConfig {
        ModelProfileConfig {
            id: "default".to_string(),
            name: "Default".to_string(),
            description: None,
            provider: None,
            model: "gpt-4o".to_string(),
            base_url: Some("https://api.openai.com/v1".to_string()),
            thinking: None,
            reasoning_effort: None,
            max_output_tokens: 4096,
            context_tokens: 128000,
            supports_images: true,
            legacy_short_term_tokens: None,
            overflow_bytes: 16384,
            auto_compress: true,
            extra_options: serde_json::Map::new(),
        }
    }

    #[test]
    fn model_input_token_estimate_handles_ascii_and_cjk() {
        assert_eq!(estimate_model_input_tokens(""), 0);
        assert_eq!(estimate_model_input_tokens("abcd"), 1);
        assert_eq!(estimate_model_input_tokens("abcde"), 2);
        assert_eq!(estimate_model_input_tokens("你好"), 2);
        assert_eq!(estimate_model_input_tokens("hi你"), 2);
    }

    #[test]
    fn memory_compaction_budget_accounts_for_output_margin_and_fixed_prompt() {
        let mut profile = test_model_profile();
        profile.context_tokens = 100_000;
        profile.max_output_tokens = 30_000;

        // 80% would be 80k, but output + 5% safety caps the request at 65k.
        // The memory share then excludes the fixed system prompt and protocol.
        let fixed = "x".repeat(4_000); // 1,000 estimated tokens
        assert_eq!(
            memory_compaction_budget_tokens(&profile, 80, &fixed),
            63_488
        );
    }

    #[test]
    fn full_request_budget_uses_percent_and_output_safety_limit() {
        let mut profile = test_model_profile();
        profile.context_tokens = 100_000;
        profile.max_output_tokens = 20_000;
        assert_eq!(model_request_budget_tokens(&profile, 80), 75_000);
        assert_eq!(model_request_budget_tokens(&profile, 60), 60_000);
    }

    #[test]
    fn model_input_snapshot_classifies_core_segments() {
        let mut assistant = Message::assistant("calling tool");
        assistant.tool_calls = Some(vec![ToolCallMessage {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "fs_read".to_string(),
                arguments: serde_json::to_string(&json!({"path": "README.md"})).unwrap(),
            },
        }]);
        let mut tool = Message::assistant("file contents");
        tool.role = Role::Tool;
        tool.tool_call_id = Some("call_1".to_string());
        let input = LoopInput::start("current")
            .history(vec![
                Message::system("system"),
                Message::system("Skill `demo` loaded for this turn from test.\n\nbody"),
                Message::user("previous"),
                assistant,
                tool,
            ])
            .metadata(json!({"thread_id": "thread"}))
            .user_state(json!({"todos": []}));
        let snapshot = model_input_snapshot_from_loop_input(
            &input,
            &[],
            "thread",
            Some("run_1"),
            "default",
            "gpt-test",
        )
        .expect("start input should produce a snapshot");
        let categories = snapshot
            .segments
            .iter()
            .map(|segment| segment.category.clone())
            .collect::<Vec<_>>();
        assert!(categories.contains(&ModelInputSegmentCategory::SystemPrompt));
        assert!(categories.contains(&ModelInputSegmentCategory::SkillInjection));
        assert!(categories.contains(&ModelInputSegmentCategory::History));
        assert!(categories.contains(&ModelInputSegmentCategory::ToolInput));
        assert!(categories.contains(&ModelInputSegmentCategory::ToolOutput));
        assert!(categories.contains(&ModelInputSegmentCategory::CurrentUser));
        assert!(categories.contains(&ModelInputSegmentCategory::Metadata));
        assert!(categories.contains(&ModelInputSegmentCategory::UserState));
        assert_eq!(snapshot.run_id, "run_1");
        assert!(snapshot.totals.estimated_tokens > 0);
    }

    #[test]
    fn background_task_completion_steer_input_includes_result_context() {
        let task = crate::ToolTaskRecord {
            task_id: "task-1".to_string(),
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args: json!({"command": "sleep 60"}),
            status: crate::tool_tasks::TOOL_TASK_COMPLETED.to_string(),
            background: true,
            started_at: "2026-07-09T00:00:00Z".to_string(),
            completed_at: Some("2026-07-09T00:01:00Z".to_string()),
            elapsed_ms: Some(60_000),
            success: Some(true),
            result_preview: Some("done".to_string()),
            recent_output: vec!["last line".to_string()],
            message: None,
            notify_on_finish: true,
            notification_delivered: false,
        };

        let steer = background_task_completion_steer_input(&task);
        let text = steer.content.text_content();

        assert_eq!(steer.id, "tool-task-task-1");
        assert!(text.contains("Background tool task completed."));
        assert!(text.contains("tool: bash"));
        assert!(text.contains("task_id: task-1"));
        assert!(text.contains("args: {\"command\":\"sleep 60\"}"));
        assert!(text.contains("elapsed_ms: 60000"));
        assert!(text.contains("last line"));
        assert!(text.contains("done"));
        assert_eq!(
            steer
                .message_metadata
                .as_ref()
                .and_then(|metadata| metadata.get("args"))
                .and_then(|args| args.as_str()),
            Some("{\"command\":\"sleep 60\"}")
        );
        assert_eq!(steer.user_name, None);
    }

    #[test]
    fn background_task_completion_steer_input_truncates_args_preview() {
        let task = crate::ToolTaskRecord {
            task_id: "task-1".to_string(),
            thread_id: "thread-1".to_string(),
            run_id: "run-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            args: json!({"command": "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"}),
            status: crate::tool_tasks::TOOL_TASK_COMPLETED.to_string(),
            background: true,
            started_at: "2026-07-09T00:00:00Z".to_string(),
            completed_at: Some("2026-07-09T00:01:00Z".to_string()),
            elapsed_ms: Some(60_000),
            success: Some(true),
            result_preview: None,
            recent_output: Vec::new(),
            message: None,
            notify_on_finish: true,
            notification_delivered: false,
        };

        let steer = background_task_completion_steer_input(&task);
        let expected = "{\"command\":\"abcdefghijklmnopqrstuvwxyz0123456789AB";

        assert_eq!(expected.chars().count(), 50);
        assert!(steer
            .content
            .text_content()
            .contains(&format!("args: {expected}")));
        assert_eq!(
            steer
                .message_metadata
                .as_ref()
                .and_then(|metadata| metadata.get("args"))
                .and_then(|args| args.as_str()),
            Some(expected)
        );
    }

    #[tokio::test]
    async fn completed_tool_task_buffer_is_drained_before_running_check() {
        let data_dir = tempfile::tempdir().unwrap();
        let manager = crate::ToolTaskManager::load(data_dir.path()).unwrap();
        let mut completed_rx = manager.subscribe_completed("thread-1").await;
        let task_id = manager
            .start(
                "thread-1".to_string(),
                "run-1".to_string(),
                "call-1".to_string(),
                "bash".to_string(),
                json!({"command": "sleep 30"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        manager.enable_completion_notification(&task_id).await;
        manager
            .finish(&task_id, true, 30_000, "done".to_string())
            .await
            .unwrap();

        assert!(!manager.is_thread_running("thread-1").await);
        let completed = try_recv_completed_tool_task(&mut completed_rx, "thread-1")
            .expect("completion event should remain buffered after task stops running");
        assert_eq!(completed.task_id, task_id);
        assert_eq!(completed.status, crate::tool_tasks::TOOL_TASK_COMPLETED);
    }

    #[tokio::test]
    async fn background_side_event_buffer_is_drained_before_running_check() {
        let data_dir = tempfile::tempdir().unwrap();
        let manager = crate::ToolTaskManager::load(data_dir.path()).unwrap();
        let mut side_rx = manager.subscribe_side_events("thread-1").await;
        manager
            .publish_side_event("thread-1".to_string(), CatEvent::Text("side".to_string()))
            .await;

        assert!(!manager.is_thread_running("thread-1").await);
        let event = try_recv_background_side_event(&mut side_rx, "thread-1")
            .expect("side event should remain buffered after task stops running");
        assert!(matches!(event, CatEvent::Text(text) if text == "side"));
    }

    #[test]
    fn system_prompt_agent_md_notice_is_added_when_cwd_contains_agent_md() {
        let data_dir = tempfile::tempdir().unwrap();
        std::fs::write(data_dir.path().join("Agent.md"), "Read me.").unwrap();

        let prompt = system_prompt_with_agent_md_notice("base prompt".to_string(), data_dir.path());

        assert!(prompt.contains("base prompt"));
        assert!(prompt.contains(AGENT_MD_CWD_SYSTEM_PROMPT_NOTICE));
    }

    #[test]
    fn system_prompt_agent_md_notice_is_omitted_without_agent_md() {
        let data_dir = tempfile::tempdir().unwrap();

        let prompt = system_prompt_with_agent_md_notice("base prompt".to_string(), data_dir.path());

        assert_eq!(prompt, "base prompt");
    }

    #[test]
    fn system_prompt_agent_md_notice_is_idempotent() {
        let data_dir = tempfile::tempdir().unwrap();
        std::fs::write(data_dir.path().join("Agent.md"), "Read me.").unwrap();
        let once = system_prompt_with_agent_md_notice("base prompt".to_string(), data_dir.path());

        let twice = system_prompt_with_agent_md_notice(once, data_dir.path());

        assert_eq!(twice.matches(AGENT_MD_CWD_SYSTEM_PROMPT_NOTICE).count(), 1);
    }

    #[test]
    fn build_installs_embedded_delegate_agent_profiles() {
        let data_dir = tempfile::tempdir().unwrap();
        let skills_dir = data_dir.path().join("skills");
        let agents_dir = data_dir.path().join("agents");
        let models_dir = data_dir.path().join("models");
        install_embedded_model_profiles(&models_dir).unwrap();

        let bot = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile: test_model_profile(),
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.path().to_path_buf(),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: data_dir.path().to_path_buf(),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: vec!["explorer".to_string()],
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir: agents_dir.clone(),
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
            acp_client_tools: None,
        }
        .build()
        .unwrap();

        assert!(bot
            .tool_list()
            .iter()
            .any(|(name, _)| name == "agent__explorer"));
        assert!(agents_dir.join("explorer.md").exists());
    }

    #[test]
    fn build_caches_pinned_skill_summaries_until_next_bot_build() {
        std::env::set_var("OPENAI_API_KEY", "test");
        let data_dir = tempfile::tempdir().unwrap();
        let skills_dir = data_dir.path().join("skills");
        let skill_dir = skills_dir.join("pinned");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pinned\ndescription: Original pinned skill\npin: true\n---\n\nBody",
        )
        .unwrap();
        let agents_dir = data_dir.path().join("agents");
        let models_dir = data_dir.path().join("models");
        install_embedded_model_profiles(&models_dir).unwrap();

        let build_bot = || {
            CatBotBuilder {
                api_key: "test".to_string(),
                model_profile: test_model_profile(),
                runtime_model_locked: false,
                system: default_system_prompt(),
                skills_dir: skills_dir.clone(),
                data_dir: data_dir.path().to_path_buf(),
                agent_md_path: None,
                overflow_bytes: None,
                memory_days: 7,
                sandbox_config: SandboxConfig::Disabled {
                    host_dir: data_dir.path().to_path_buf(),
                },
                im_bridge: None,
                extra_options: serde_json::Map::new(),
                tool_allowlist: None,
                delegate_ids: Vec::new(),
                active_agent_id: DEFAULT_AGENT_ID.to_string(),
                model_bindings: AgentModelBindings::default(),
                approval_model_profile_id: None,
                agents_dir: agents_dir.clone(),
                max_turns: Some(2),
                model_registry: Arc::new(ModelProfileRegistry::load(models_dir.clone()).unwrap()),
                acp_client_tools: None,
            }
            .build()
            .unwrap()
        };

        let bot = build_bot();
        assert_eq!(bot.pinned_skill_summaries.len(), 1);
        assert_eq!(
            bot.pinned_skill_summaries[0].description,
            "Original pinned skill"
        );

        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: pinned\ndescription: Updated pinned skill\npin: true\n---\n\nBody",
        )
        .unwrap();
        assert_eq!(
            bot.pinned_skill_summaries[0].description,
            "Original pinned skill"
        );

        let rebuilt = build_bot();
        assert_eq!(
            rebuilt.pinned_skill_summaries[0].description,
            "Updated pinned skill"
        );
    }

    #[test]
    fn persistent_delegate_agent_exposes_named_parameter() {
        std::env::set_var("OPENAI_API_KEY", "test");
        let data_dir = tempfile::tempdir().unwrap();
        let skills_dir = data_dir.path().join("skills");
        let agents_dir = data_dir.path().join("agents");
        let models_dir = data_dir.path().join("models");
        install_embedded_model_profiles(&models_dir).unwrap();
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("coder.md"),
            r#"---
id: coder
name: Coder
description: Persistent coder
tools:
  - search
persistent_sessions: true
---
You are Coder.
"#,
        )
        .unwrap();

        let root_profile = crate::profile::AgentProfile::from_markdown(
            r#"---
id: default
name: Remi
description: General assistant
delegates:
  - coder
---
You are Remi.
"#,
        )
        .unwrap();

        let bot = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile: test_model_profile(),
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.path().to_path_buf(),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: data_dir.path().to_path_buf(),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
            acp_client_tools: None,
        }
        .agent_profile(root_profile)
        .unwrap()
        .build()
        .unwrap();

        let definitions = super::cat_agent_tool_definitions(&bot.inner);
        let coder = definitions
            .into_iter()
            .find(|definition| definition.function.name == "agent__coder")
            .expect("coder delegate should be registered");
        assert!(
            coder
                .function
                .parameters
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|properties| properties.contains_key("named")),
            "persistent delegate schema should expose named"
        );
    }

    #[test]
    fn subagent_named_argument_is_validated() {
        assert_eq!(
            RemiSubAgentTool::named_from_args(&json!({"task": "work"})).unwrap(),
            None
        );
        assert_eq!(
            RemiSubAgentTool::named_from_args(&json!({"task": "work", "named": "build_1"}))
                .unwrap(),
            Some("build_1".to_string())
        );
        assert!(
            RemiSubAgentTool::named_from_args(&json!({"task": "work", "named": "bad name"}))
                .is_err()
        );
    }

    #[test]
    fn subagent_tool_result_returns_sub_session_id() {
        let text = format_subagent_tool_result(
            &ThreadId("subagent:explorer:test".to_string()),
            &RunId("run-1".to_string()),
            "explorer",
            true,
            "done",
            None,
        );
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(value["sub_session_id"], "subagent:explorer:test");
        assert_eq!(value["sub_run_id"], "run-1");
        assert_eq!(value["agent"], "explorer");
        assert_eq!(value["success"], true);
        assert_eq!(value["final_output"], "done");
    }

    #[test]
    fn partial_turn_recorder_synthesizes_safe_cancel_history() {
        let mut recorder = PartialTurnRecorder::new(vec![
            Message::system("system"),
            Message::user("inspect repo"),
        ]);
        recorder.on_text("I found the issue.");
        recorder.on_tool_start("call_1".to_string(), "read_file".to_string());
        recorder.on_tool_arguments_delta("call_1", "{\"path\":\"src/lib.rs\"");

        let history = recorder
            .synthesize_history()
            .expect("partial history should be synthesized");
        let assistant = history.last().expect("assistant message should exist");

        assert_eq!(history.len(), 3);
        assert!(matches!(assistant.role, Role::Assistant));
        assert!(assistant.tool_calls.is_none());
        assert!(assistant.tool_call_id.is_none());
        let content = assistant.content.text_content();
        assert!(content.contains("I found the issue."));
        assert!(content.contains("[Cancelled tool activity]"));
        assert!(content.contains("read_file (call_1)"));
        assert!(content.contains("cancelled before completion"));
    }

    #[tokio::test]
    async fn cancelled_stream_persists_partial_assistant_text() {
        std::env::set_var("OPENAI_API_KEY", "test");
        let (base_url, _requests) = start_slow_openai_mock_server("partial answer").await;
        let data_dir = tempfile::tempdir().unwrap();
        let skills_dir = data_dir.path().join("skills");
        let agents_dir = data_dir.path().join("agents");
        let models_dir = data_dir.path().join("models");
        install_embedded_model_profiles(&models_dir).unwrap();
        let mut model_profile = test_model_profile();
        model_profile.base_url = Some(base_url);
        model_profile.model = "mock-model".to_string();
        let bot = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile,
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.path().to_path_buf(),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: data_dir.path().to_path_buf(),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
            acp_client_tools: None,
        }
        .build()
        .unwrap();

        let cancel = CancellationToken::new();
        let mut stream = std::pin::pin!(bot.stream_with_options(
            "cancel-partial-thread",
            Content::text("start"),
            StreamOptions {
                cancel: Some(cancel.clone()),
                ..StreamOptions::default()
            },
        ));
        let mut saw_partial = false;
        while let Some(event) = stream.next().await {
            match event {
                CatEvent::Text(text) => {
                    if text.contains("partial answer") {
                        saw_partial = true;
                        cancel.cancel();
                    }
                }
                CatEvent::Done => break,
                CatEvent::Error(err) => panic!("stream error: {err}"),
                _ => {}
            }
        }

        assert!(saw_partial);
        let persisted = tokio::fs::read_to_string(
            data_dir
                .path()
                .join("memory/cancel-partial-thread/short_term.jsonl"),
        )
        .await
        .expect("partial turn should be persisted");
        assert!(persisted.contains("start"));
        assert!(persisted.contains("partial answer"));
    }

    #[tokio::test]
    async fn cancelled_stream_pauses_active_supervisor_without_evaluation() {
        std::env::set_var("OPENAI_API_KEY", "test");
        let (base_url, requests) = start_slow_openai_mock_server("partial answer").await;
        let data_dir = tempfile::tempdir().unwrap();
        let skills_dir = data_dir.path().join("skills");
        let agents_dir = data_dir.path().join("agents");
        let models_dir = data_dir.path().join("models");
        install_embedded_model_profiles(&models_dir).unwrap();
        let mut model_profile = test_model_profile();
        model_profile.base_url = Some(base_url);
        model_profile.model = "mock-model".to_string();
        let bot = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile,
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.path().to_path_buf(),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: data_dir.path().to_path_buf(),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
            acp_client_tools: None,
        }
        .build()
        .unwrap();

        let thread_id = "cancel-pauses-supervisor";
        bot.set_goal(thread_id, "pause on cancel", GoalMaxRounds::Limited(1))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let mut stream = std::pin::pin!(bot.stream_with_options(
            thread_id,
            Content::text("start"),
            StreamOptions {
                cancel: Some(cancel.clone()),
                ..StreamOptions::default()
            },
        ));
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            if let CatEvent::Text(text) = &event {
                if text.contains("partial answer") {
                    cancel.cancel();
                }
            }
            let done = matches!(event, CatEvent::Done);
            events.push(event);
            if done {
                break;
            }
        }

        assert!(events.iter().any(|event| matches!(
            event,
            CatEvent::Supervisor(report)
                if report.status == WorkflowStatus::Paused
                    && report.reason == "interrupted by user"
        )));
        let instance = bot.workflow_status(thread_id).await.unwrap();
        assert_eq!(instance.status, WorkflowStatus::Paused);
        assert_eq!(
            instance.pause_reason,
            Some(supervisor_workflow::WorkflowPauseReason::Interrupted)
        );
        assert_eq!(requests.lock().expect("request lock poisoned").len(), 1);
    }

    #[tokio::test]
    async fn cancelled_supervisor_evaluation_pauses_without_waiting_for_timeout() {
        std::env::set_var("OPENAI_API_KEY", "test");
        let (base_url, requests) = start_openai_mock_server_with_slow_tail(
            vec![sse_text("main done")],
            "supervisor partial",
        )
        .await;
        let data_dir = tempfile::tempdir().unwrap();
        let skills_dir = data_dir.path().join("skills");
        let agents_dir = data_dir.path().join("agents");
        let models_dir = data_dir.path().join("models");
        install_embedded_model_profiles(&models_dir).unwrap();
        let mut model_profile = test_model_profile();
        model_profile.base_url = Some(base_url);
        model_profile.model = "mock-model".to_string();
        let bot = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile,
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.path().to_path_buf(),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: data_dir.path().to_path_buf(),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
            acp_client_tools: None,
        }
        .build()
        .unwrap();

        let thread_id = "cancel-supervisor-evaluation";
        bot.set_goal(
            thread_id,
            "cancel supervisor evaluation",
            GoalMaxRounds::Limited(1),
        )
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        let started = Instant::now();
        let mut stream = std::pin::pin!(bot.stream_with_options(
            thread_id,
            Content::text("start"),
            StreamOptions {
                cancel: Some(cancel.clone()),
                ..StreamOptions::default()
            },
        ));
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for event: {events:?}"));
            let Some(event) = event else {
                break;
            };
            if matches!(
                &event,
                CatEvent::SupervisorProgress(SupervisorTraceEvent::OutputDelta { content })
                    if content.contains("supervisor partial")
            ) {
                cancel.cancel();
            }
            let done = matches!(event, CatEvent::Done);
            events.push(event);
            if done {
                break;
            }
        }

        assert!(
            started.elapsed() < Duration::from_secs(4),
            "supervisor cancel should not wait for slow response or timeout: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            CatEvent::Supervisor(report)
                if report.status == WorkflowStatus::Paused
                    && report.reason == "interrupted by user"
        )));
        let instance = bot.workflow_status(thread_id).await.unwrap();
        assert_eq!(instance.status, WorkflowStatus::Paused);
        assert_eq!(
            instance.pause_reason,
            Some(supervisor_workflow::WorkflowPauseReason::Interrupted)
        );
        assert_eq!(requests.lock().expect("request lock poisoned").len(), 2);
    }

    #[tokio::test]
    async fn user_denied_approval_pauses_supervisor_and_next_input_resumes() {
        std::env::set_var("OPENAI_API_KEY", "test");
        let responses = vec![
            sse_tool_call(
                "call_write",
                "fs_write",
                json!({
                    "path": "deny.txt",
                    "content": "denied"
                }),
            ),
            sse_text("after resume"),
            sse_text(
                r#"{"edge":"complete","agent_message":null,"next_node_message":null,"reason":"resumed and completed"}"#,
            ),
        ];
        let (base_url, requests) = start_openai_mock_server(responses).await;
        let data_dir = tempfile::tempdir().unwrap();
        let skills_dir = data_dir.path().join("skills");
        let agents_dir = data_dir.path().join("agents");
        let models_dir = data_dir.path().join("models");
        install_embedded_model_profiles(&models_dir).unwrap();
        let mut model_profile = test_model_profile();
        model_profile.base_url = Some(base_url);
        model_profile.model = "mock-model".to_string();
        let bot = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile,
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.path().to_path_buf(),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: data_dir.path().to_path_buf(),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir,
            max_turns: Some(4),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
            acp_client_tools: None,
        }
        .build()
        .unwrap();

        let thread_id = "approval-deny-pauses-supervisor";
        bot.set_goal(
            thread_id,
            "pause on approval deny",
            GoalMaxRounds::Limited(1),
        )
        .await
        .unwrap();
        let mut stream = Box::pin(bot.stream_with_options(
            thread_id,
            Content::text("try risky command"),
            StreamOptions {
                platform: Some("tui".to_string()),
                ..StreamOptions::default()
            },
        ));
        let mut first_events = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap_or_else(|_| {
                    panic!("timed out waiting for first-run event: {first_events:?}")
                });
            let Some(event) = event else {
                break;
            };
            if let CatEvent::ToolApprovalRequested(request) = &event {
                bot.approval_manager()
                    .decide(&request.id, crate::ToolApprovalDecision::Deny)
                    .await;
            }
            let done = matches!(event, CatEvent::Done);
            first_events.push(event);
            if done {
                break;
            }
        }
        drop(stream);

        assert!(first_events.iter().any(|event| matches!(
            event,
            CatEvent::ToolApprovalResolved { decision, .. }
                if *decision == crate::ToolApprovalDecision::Deny
        )));
        assert!(first_events.iter().any(|event| matches!(
            event,
            CatEvent::Supervisor(report)
                if report.status == WorkflowStatus::Paused
                    && report.reason == "tool approval denied by user"
        )));
        assert!(!first_events.iter().any(
            |event| matches!(event, CatEvent::ToolCallResult { name, .. } if name == "fs_write")
        ));
        assert_eq!(requests.lock().expect("request lock poisoned").len(), 1);
        let instance = bot.workflow_status(thread_id).await.unwrap();
        assert_eq!(instance.status, WorkflowStatus::Paused);
        assert_eq!(
            instance.pause_reason,
            Some(supervisor_workflow::WorkflowPauseReason::Interrupted)
        );

        let mut second_stream = Box::pin(bot.stream(thread_id, "continue after denial"));
        let mut second_events = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), second_stream.next())
                .await
                .unwrap_or_else(|_| {
                    let request_count = requests.lock().expect("request lock poisoned").len();
                    panic!(
                        "timed out waiting for resume-run event after {request_count} requests: {second_events:?}"
                    )
                });
            let Some(event) = event else {
                break;
            };
            if let CatEvent::Error(err) = &event {
                panic!("resume stream error: {err}");
            }
            let done = matches!(event, CatEvent::Done);
            second_events.push(event);
            if done {
                break;
            }
        }
        assert!(second_events.iter().any(|event| matches!(
            event,
            CatEvent::Supervisor(report)
                if report.status == WorkflowStatus::Active
                    && report.reason == "user input received; resuming workflow"
        )));
        assert!(second_events
            .iter()
            .any(|event| matches!(event, CatEvent::Text(text) if text.contains("after resume"))));
        assert!(second_events.iter().any(|event| matches!(
            event,
            CatEvent::Supervisor(report)
                if report.status == WorkflowStatus::Completed
                    && report.reason == "resumed and completed"
        )));
        let instance = bot.workflow_status(thread_id).await.unwrap();
        assert_eq!(instance.status, WorkflowStatus::Completed);
        assert_eq!(requests.lock().expect("request lock poisoned").len(), 3);
    }

    #[test]
    fn context_percent_tokens_defaults_to_eighty_percent_of_context() {
        let profile = test_model_profile();
        assert_eq!(
            context_percent_tokens(&profile, DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT),
            102_400
        );
    }

    #[test]
    fn context_percent_tokens_rounds_up_and_never_returns_zero() {
        let mut profile = test_model_profile();
        profile.context_tokens = 1;
        assert_eq!(context_percent_tokens(&profile, 80), 1);
    }

    #[tokio::test]
    async fn thread_run_lock_is_scoped_by_thread_id() {
        let locks: ThreadRunLocks = Arc::new(AsyncMutex::new(HashMap::new()));

        let first = thread_run_lock(&locks, "session-a").await;
        let second = thread_run_lock(&locks, "session-a").await;
        let other = thread_run_lock(&locks, "session-b").await;

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other));
    }

    #[test]
    fn workflow_round_limit_allows_default_twenty_continuations() {
        assert!(super::workflow_round_allows_continue(
            &GoalMaxRounds::Limited(20),
            0
        ));
        assert!(super::workflow_round_allows_continue(
            &GoalMaxRounds::Limited(20),
            19
        ));
        assert!(!super::workflow_round_allows_continue(
            &GoalMaxRounds::Limited(20),
            20
        ));
        assert!(super::workflow_round_allows_continue(
            &GoalMaxRounds::Unlimited,
            u32::MAX
        ));
    }

    #[test]
    fn agent_profile_delegates_are_exposed_as_tools() {
        let profile = crate::profile::AgentProfile::from_markdown(
            r#"---
id: default
name: Remi
description: General assistant
tools:
  - search
delegates:
  - coder
---
You are Remi.
"#,
        )
        .unwrap();
        let builder = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile: test_model_profile(),
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir: PathBuf::from("skills"),
            data_dir: PathBuf::from("data"),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: PathBuf::from("data"),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir: PathBuf::from("agents"),
            max_turns: None,
            model_registry: Arc::new(ModelProfileRegistry::load(PathBuf::from("models")).unwrap()),
            acp_client_tools: None,
        }
        .agent_profile(profile)
        .unwrap();
        let tools = builder.tool_allowlist.unwrap();
        assert!(tools.iter().any(|tool| tool == "search"));
        assert!(tools.iter().any(|tool| tool == "agent__coder"));
        assert_eq!(builder.delegate_ids, vec!["coder"]);
    }

    #[test]
    fn explicit_runtime_model_profile_is_not_overridden_by_agent_primary_model() {
        let profile = crate::profile::AgentProfile::from_markdown(
            r#"---
id: default
name: Remi
description: General assistant
models:
  primary: default
---
You are Remi.
"#,
        )
        .unwrap();
        let builder = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile: ModelProfileConfig {
                id: "deepseek-v4-flash".to_string(),
                name: "DeepSeek V4 Flash".to_string(),
                description: None,
                provider: None,
                model: "deepseek-v4-flash".to_string(),
                base_url: Some("https://api.deepseek.com".to_string()),
                thinking: None,
                reasoning_effort: None,
                max_output_tokens: 393216,
                context_tokens: 1_000_000,
                supports_images: false,
                legacy_short_term_tokens: None,
                overflow_bytes: 24000,
                auto_compress: true,
                extra_options: serde_json::Map::new(),
            },
            runtime_model_locked: true,
            system: default_system_prompt(),
            skills_dir: PathBuf::from("skills"),
            data_dir: PathBuf::from("data"),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: PathBuf::from("data"),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir: PathBuf::from("agents"),
            max_turns: None,
            model_registry: Arc::new(ModelProfileRegistry::load(PathBuf::from("models")).unwrap()),
            acp_client_tools: None,
        }
        .agent_profile(profile)
        .unwrap();
        assert_eq!(builder.model_profile.id, "deepseek-v4-flash");
        assert_eq!(builder.model_profile.model, "deepseek-v4-flash");
        assert_eq!(
            builder.model_profile.base_url.as_deref(),
            Some("https://api.deepseek.com")
        );
    }

    #[test]
    fn build_injects_local_acp_runner() {
        let data_dir = std::env::temp_dir().join(format!("remi-cat-acp-runner-{}", Uuid::new_v4()));
        let skills_dir = data_dir.join("skills");
        let agents_dir = data_dir.join("agents");
        let models_dir = data_dir.join("models");
        let builder = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile: test_model_profile(),
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.clone(),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: data_dir.clone(),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
            acp_client_tools: None,
        };

        let bot = builder.build().unwrap();
        assert!(bot.acp_backend.has_local_runner());

        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn local_acp_thread_id_is_session_scoped() {
        assert_eq!(local_acp_thread_id("session-1"), "acp:session-1");
    }

    #[test]
    fn group_text_content_is_prefixed_with_sender_username() {
        let content =
            prepend_group_sender_username(Content::text("hello"), Some("group"), Some("vv"));

        match content {
            Content::Text(text) => assert_eq!(text, "vv:\nhello"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn p2p_content_is_not_prefixed() {
        let content =
            prepend_group_sender_username(Content::text("hello"), Some("p2p"), Some("vv"));

        match content {
            Content::Text(text) => assert_eq!(text, "hello"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn group_multimodal_content_gets_prefix_text_part() {
        let content = prepend_group_sender_username(
            Content::parts(vec![ContentPart::image_url("data:image/png;base64,abc")]),
            Some("group"),
            Some("vv"),
        );

        match content {
            Content::Parts(parts) => {
                assert!(
                    matches!(parts.first(), Some(ContentPart::Text { text }) if text == "vv:\n")
                );
                assert!(matches!(parts.get(1), Some(ContentPart::ImageUrl { .. })));
            }
            other => panic!("expected parts content, got {other:?}"),
        }
    }

    #[test]
    fn single_chat_sender_system_prompt_is_inserted_after_soul() {
        let mut history = vec![
            Message::system("agent"),
            Message::system("soul"),
            Message::system("long-term"),
        ];

        insert_single_chat_sender_system_prompt(
            &mut history,
            2,
            single_chat_sender_system_prompt(Some("p2p"), Some("Alice"), Some("uuid-1")),
        );

        let contents: Vec<String> = history
            .iter()
            .map(|message| message.content.text_content())
            .collect();
        assert_eq!(contents[0], "agent");
        assert_eq!(contents[1], "soul");
        assert_eq!(
            contents[2],
            "当前是单聊场景。当前正在与你对话的用户是 Alice（内部ID: uuid-1）。"
        );
        assert_eq!(contents[3], "long-term");
    }

    #[test]
    fn async_tool_system_prompt_is_inserted_after_skill_prompts() {
        let mut history = vec![
            Message::system("agent"),
            Message::system("skill"),
            Message::system("long-term"),
        ];

        insert_async_tool_system_prompt(&mut history, 2);

        let contents: Vec<String> = history
            .iter()
            .map(|message| message.content.text_content())
            .collect();
        assert_eq!(contents[0], "agent");
        assert_eq!(contents[1], "skill");
        assert_eq!(contents[2], ASYNC_TOOL_SYSTEM_PROMPT);
        assert_eq!(contents[3], "long-term");
    }

    #[test]
    fn group_chat_does_not_get_single_chat_sender_system_prompt() {
        assert!(
            single_chat_sender_system_prompt(Some("group"), Some("Alice"), Some("uuid-1"))
                .is_none()
        );
    }

    #[test]
    fn single_chat_sender_system_prompt_uses_uuid_when_username_missing() {
        let prompt = single_chat_sender_system_prompt(Some("p2p"), None, Some("uuid-1"));
        assert_eq!(
            prompt.as_deref(),
            Some("当前是单聊场景。当前正在与你对话的用户内部ID是 uuid-1。")
        );
    }

    #[test]
    fn thread_todo_batch_prompt_is_appended_after_history_context() {
        let user_state = json!({
            "__todos": [TodoItem {
                id: 1,
                content: "Draft changelog".to_string(),
                description: None,
                done: false,
                batch_id: Some(1),
                batch_title: Some("Release launch".to_string()),
                batch_index: Some(0),
            }]
        });
        let ctx = MemoryContext {
            agent_md: Some("agent".to_string()),
            soul_md: Some("soul".to_string()),
            long_term: MemoryIndex::default(),
            mid_term: MemoryIndex::default(),
            short_term: vec![
                Message::user("previous user"),
                Message::assistant("previous assistant"),
            ],
            latest_summary: None,
            user_state,
        };

        let mut history = build_injected_history(&ctx);
        insert_single_chat_sender_system_prompt(
            &mut history,
            2,
            single_chat_sender_system_prompt(Some("p2p"), Some("Alice"), Some("uuid-1")),
        );
        append_thread_todo_system_prompt(&mut history, &ctx.user_state);

        let contents: Vec<String> = history
            .iter()
            .map(|message| message.content.text_content())
            .collect();
        assert_eq!(contents[0], "agent");
        assert_eq!(contents[1], "soul");
        assert_eq!(
            contents[2],
            "当前是单聊场景。当前正在与你对话的用户是 Alice（内部ID: uuid-1）。"
        );
        assert_eq!(contents[3], "previous user");
        assert_eq!(contents[4], "previous assistant");
        assert_eq!(
            contents[5],
            "[CURRENT TODO BATCH]\nThis thread still has unfinished work under \"Release launch\".\nKeep progress synchronized with todo__complete/update/remove.\nWhen this thread has an active plan, try to complete multiple todo items in one pass whenever feasible. Only stop early if the user explicitly cancels, changes direction, or you need user input/help to proceed. Finish each individual todo item's work before marking it complete.\n- #1 Draft changelog"
        );
    }

    #[test]
    fn active_supervisor_receives_todo_instead_of_main_agent_history() {
        let user_state = json!({
            "__todos": [TodoItem {
                id: 1,
                content: "Run verification".to_string(),
                description: None,
                done: false,
                batch_id: Some(1),
                batch_title: Some("Release".to_string()),
                batch_index: Some(0),
            }]
        });

        let mut supervised_history = vec![Message::user("start")];
        let supervisor_todo = route_thread_todo_prompt(&mut supervised_history, &user_state, true);
        assert_eq!(supervised_history.len(), 1);
        assert!(supervisor_todo
            .as_deref()
            .is_some_and(|prompt| prompt.contains("Run verification")));

        let mut main_agent_history = vec![Message::user("start")];
        let supervisor_todo = route_thread_todo_prompt(&mut main_agent_history, &user_state, false);
        assert!(supervisor_todo.is_none());
        assert_eq!(main_agent_history.len(), 2);
        assert!(main_agent_history[1]
            .content
            .text_content()
            .contains("Run verification"));
    }

    #[test]
    fn active_supervisor_routes_existing_todo_only_to_supervisor_end_to_end() {
        run_large_stack_local_test(|| async {
            std::env::set_var("OPENAI_API_KEY", "test");
            let responses = vec![
                sse_tool_call(
                    "call_add",
                    "todo__add",
                    json!({
                        "title": "Supervisor routing",
                        "items": [
                            {"title": "检查主agent不直接接收todo"},
                            {"title": "确认supervisor接收todo"}
                        ]
                    }),
                ),
                sse_text("todo created"),
                sse_text("main agent ran without direct todo injection"),
                sse_text(
                    r#"{"edge":"complete","agent_message":null,"next_node_message":null,"reason":"todo routing verified"}"#,
                ),
            ];
            let (base_url, requests) = start_openai_mock_server(responses).await;
            let data_dir = tempfile::tempdir().unwrap();
            let skills_dir = data_dir.path().join("skills");
            let agents_dir = data_dir.path().join("agents");
            let models_dir = data_dir.path().join("models");
            install_embedded_model_profiles(&models_dir).unwrap();
            let mut model_profile = test_model_profile();
            model_profile.base_url = Some(base_url);
            model_profile.model = "mock-model".to_string();
            let bot = CatBotBuilder {
                api_key: "test".to_string(),
                model_profile,
                runtime_model_locked: false,
                system: default_system_prompt(),
                skills_dir,
                data_dir: data_dir.path().to_path_buf(),
                agent_md_path: None,
                overflow_bytes: None,
                memory_days: 7,
                sandbox_config: SandboxConfig::Disabled {
                    host_dir: data_dir.path().to_path_buf(),
                },
                im_bridge: None,
                extra_options: serde_json::Map::new(),
                tool_allowlist: None,
                delegate_ids: Vec::new(),
                active_agent_id: DEFAULT_AGENT_ID.to_string(),
                model_bindings: AgentModelBindings::default(),
                approval_model_profile_id: None,
                agents_dir,
                max_turns: Some(8),
                model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
                acp_client_tools: None,
            }
            .build()
            .unwrap();

            let thread_id = "todo-supervisor-routing-e2e";
            bot.approval_manager().grant_session(thread_id).await;
            collect_stream(bot.stream(thread_id, "create todo")).await;
            bot.set_goal(thread_id, "verify todo routing", GoalMaxRounds::Limited(1))
                .await
                .unwrap();
            let events = collect_stream(bot.stream(thread_id, "continue")).await;
            assert!(events
            .iter()
            .any(|event| matches!(event, CatEvent::Supervisor(report) if report.reason == "todo routing verified")));

            let requests = requests.lock().expect("request lock poisoned");
            assert_eq!(requests.len(), 4);
            assert!(
                !requests[2].contains("[CURRENT TODO BATCH]"),
                "main-agent request unexpectedly contained todo injection: {}",
                requests[2]
            );
            assert!(
                requests[3].contains("[CURRENT TODO BATCH]"),
                "supervisor request did not contain todo injection: {}",
                requests[3]
            );
            assert!(requests[3].contains("确认supervisor接收todo"));
        });
    }

    #[test]
    fn user_question_moves_workflow_through_ask_user_for_help_without_supervisor_stop() {
        run_large_stack_local_test(|| async {
            std::env::set_var("OPENAI_API_KEY", "test");
            let responses = vec![
                sse_tool_call(
                    "call_ask",
                    "ask_user_question",
                    json!({
                        "question": "Need user input?",
                        "allow_free_text": true
                    }),
                ),
                sse_text("Thanks, continuing with the answer."),
            ];
            let (base_url, requests) = start_openai_mock_server(responses).await;
            let data_dir = tempfile::tempdir().unwrap();
            let skills_dir = data_dir.path().join("skills");
            let agents_dir = data_dir.path().join("agents");
            let models_dir = data_dir.path().join("models");
            install_embedded_model_profiles(&models_dir).unwrap();
            let mut model_profile = test_model_profile();
            model_profile.base_url = Some(base_url);
            model_profile.model = "mock-model".to_string();
            let bot = CatBotBuilder {
                api_key: "test".to_string(),
                model_profile,
                runtime_model_locked: false,
                system: default_system_prompt(),
                skills_dir,
                data_dir: data_dir.path().to_path_buf(),
                agent_md_path: None,
                overflow_bytes: None,
                memory_days: 7,
                sandbox_config: SandboxConfig::Disabled {
                    host_dir: data_dir.path().to_path_buf(),
                },
                im_bridge: None,
                extra_options: serde_json::Map::new(),
                tool_allowlist: None,
                delegate_ids: Vec::new(),
                active_agent_id: DEFAULT_AGENT_ID.to_string(),
                model_bindings: AgentModelBindings::default(),
                approval_model_profile_id: None,
                agents_dir,
                max_turns: Some(8),
                model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
                acp_client_tools: None,
            }
            .build()
            .unwrap();

            let thread_id = "ask-user-supervisor-control-node";
            bot.set_goal(
                thread_id,
                "complete after user input",
                GoalMaxRounds::Limited(1),
            )
            .await
            .unwrap();
            let mut stream = std::pin::pin!(bot.stream(thread_id, "ask if needed"));
            let mut events = Vec::new();
            while let Some(event) = stream.next().await {
                if let CatEvent::Error(err) = &event {
                    panic!("stream error: {err}");
                }
                if let CatEvent::UserQuestionRequested(request) = &event {
                    assert_eq!(request.session_id, thread_id);
                    bot.answer_user_question(
                        &request.id,
                        UserQuestionResponse {
                            question_id: request.id.clone(),
                            status: UserQuestionStatus::Answered,
                            selected_option_ids: Vec::new(),
                            free_text: Some("user supplied detail".to_string()),
                            answer_text: Some("user supplied detail".to_string()),
                            answered_at: None,
                            source: Some("test".to_string()),
                        },
                    )
                    .await;
                }
                let done = matches!(event, CatEvent::Done);
                events.push(event);
                if done {
                    break;
                }
            }

            let supervisor_events = events
                .iter()
                .filter_map(|event| match event {
                    CatEvent::Supervisor(report) => Some(format!(
                        "{} -> {} ({:?}) {}",
                        report.from_node, report.to_node, report.status, report.reason
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    CatEvent::Supervisor(report)
                        if report.to_node == supervisor_workflow::ASK_USER_FOR_HELP_NODE
                            && report.status == WorkflowStatus::Paused
                )),
                "supervisor events: {supervisor_events:?}"
            );
            assert!(!events.iter().any(|event| matches!(
                event,
                CatEvent::Supervisor(report) if report.status == WorkflowStatus::Completed
            )));
            let instance = bot.workflow_status(thread_id).await.unwrap();
            assert_eq!(instance.status, WorkflowStatus::Active);
            assert_eq!(instance.current_node, "review");
            assert!(instance.ask_user_for_help.is_none());

            let requests = requests.lock().expect("request lock poisoned");
            assert_eq!(
                requests.len(),
                2,
                "supervisor evaluation should be skipped after user-question round"
            );
        });
    }

    #[tokio::test]
    async fn pinned_skill_prompt_is_sent_to_model_end_to_end() {
        std::env::set_var("OPENAI_API_KEY", "test");
        let responses = vec![sse_text("pinned prompt observed")];
        let (base_url, requests) = start_openai_mock_server(responses).await;
        let data_dir = tempfile::tempdir().unwrap();
        let skills_dir = data_dir.path().join("skills");
        let pinned_dir = skills_dir.join("always-handy");
        let unpinned_dir = skills_dir.join("quiet-skill");
        std::fs::create_dir_all(&pinned_dir).unwrap();
        std::fs::create_dir_all(&unpinned_dir).unwrap();
        std::fs::write(
            pinned_dir.join("SKILL.md"),
            "---\nname: always-handy\ndescription: Pinned discovery description\npin: true\n---\n\nPinned body must not be injected.",
        )
        .unwrap();
        std::fs::write(
            unpinned_dir.join("SKILL.md"),
            "---\nname: quiet-skill\ndescription: Unpinned discovery description\n---\n\nQuiet body.",
        )
        .unwrap();
        let agents_dir = data_dir.path().join("agents");
        let models_dir = data_dir.path().join("models");
        install_embedded_model_profiles(&models_dir).unwrap();
        let mut model_profile = test_model_profile();
        model_profile.base_url = Some(base_url);
        model_profile.model = "mock-model".to_string();
        let bot = CatBotBuilder {
            api_key: "test".to_string(),
            model_profile,
            runtime_model_locked: false,
            system: default_system_prompt(),
            skills_dir,
            data_dir: data_dir.path().to_path_buf(),
            agent_md_path: None,
            overflow_bytes: None,
            memory_days: 7,
            sandbox_config: SandboxConfig::Disabled {
                host_dir: data_dir.path().to_path_buf(),
            },
            im_bridge: None,
            extra_options: serde_json::Map::new(),
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            approval_model_profile_id: None,
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
            acp_client_tools: None,
        }
        .build()
        .unwrap();

        let events = collect_stream(bot.stream("pinned-skill-e2e", "hello")).await;
        assert!(events.iter().any(
            |event| matches!(event, CatEvent::Text(text) if text.contains("pinned prompt observed"))
        ));

        let requests = requests.lock().expect("request lock poisoned");
        assert_eq!(requests.len(), 1);
        let request: serde_json::Value = serde_json::from_str(&requests[0]).unwrap();
        let messages = request
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .expect("request should contain messages");
        let system_messages = messages
            .iter()
            .filter(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("system")
            })
            .filter_map(|message| message.get("content").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>();
        assert!(
            system_messages
                .iter()
                .any(|content| content.contains("Pinned skills are only a curated subset")),
            "request should contain updated default system prompt: {system_messages:#?}"
        );
        assert!(
            system_messages
                .iter()
                .any(|content| content.contains("Before starting substantive work, search for")
                    && content.contains("relevant skills and memory")),
            "request should tell the model to search relevant skills and memory: {system_messages:#?}"
        );
        let pinned_prompt = messages
            .iter()
            .filter(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("system")
            })
            .filter_map(|message| message.get("content").and_then(serde_json::Value::as_str))
            .find(|content| content.contains("## Pinned Skills"))
            .expect("request should contain pinned skill system prompt");
        assert!(pinned_prompt.contains("always-handy"), "{pinned_prompt}");
        assert!(
            pinned_prompt.contains("Pinned discovery description"),
            "{pinned_prompt}"
        );
        assert!(
            !pinned_prompt.contains("Pinned body must not be injected"),
            "{pinned_prompt}"
        );
        assert!(!pinned_prompt.contains("quiet-skill"), "{pinned_prompt}");
        assert!(
            !pinned_prompt.contains("Unpinned discovery description"),
            "{pinned_prompt}"
        );
        assert!(pinned_prompt.contains("`skill__search`"), "{pinned_prompt}");
        assert!(pinned_prompt.contains("`skill__get`"), "{pinned_prompt}");
        assert!(!pinned_prompt.contains("scope=skills"), "{pinned_prompt}");
    }

    #[test]
    fn search_tool_tokenized_query_matches_memory_and_skills_end_to_end() {
        run_large_stack_local_test(|| async {
            std::env::set_var("OPENAI_API_KEY", "test");
            let responses = vec![
                sse_tool_call(
                    "call_search",
                    "search",
                    json!({
                        "query": "我想找靠窗座位相关内容",
                        "scope": "local",
                        "limit": 10
                    }),
                ),
                sse_text("search observed"),
            ];
            let (base_url, requests) = start_openai_mock_server(responses).await;
            let data_dir = tempfile::tempdir().unwrap();
            let skills_dir = data_dir.path().join("skills");
            let skill_dir = skills_dir.join("window-seat");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: window-seat\ndescription: 靠窗座位偏好处理流程\n---\n\nUse this skill when seating preferences mention windows.",
        )
        .unwrap();
            let agents_dir = data_dir.path().join("agents");
            let models_dir = data_dir.path().join("models");
            install_embedded_model_profiles(&models_dir).unwrap();
            let mut model_profile = test_model_profile();
            model_profile.base_url = Some(base_url);
            model_profile.model = "mock-model".to_string();
            let bot = CatBotBuilder {
                api_key: "test".to_string(),
                model_profile,
                runtime_model_locked: false,
                system: default_system_prompt(),
                skills_dir,
                data_dir: data_dir.path().to_path_buf(),
                agent_md_path: None,
                overflow_bytes: None,
                memory_days: 7,
                sandbox_config: SandboxConfig::Disabled {
                    host_dir: data_dir.path().to_path_buf(),
                },
                im_bridge: None,
                extra_options: serde_json::Map::new(),
                tool_allowlist: None,
                delegate_ids: Vec::new(),
                active_agent_id: DEFAULT_AGENT_ID.to_string(),
                model_bindings: AgentModelBindings::default(),
                approval_model_profile_id: None,
                agents_dir,
                max_turns: Some(4),
                model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
                acp_client_tools: None,
            }
            .build()
            .unwrap();
            bot.memory
                .upsert_named_memory(
                    DEFAULT_AGENT_ID,
                    "seat-preference",
                    "用户旅行偏好：坐飞机时优先选择靠窗位置。",
                )
                .await
                .unwrap();

            let events =
                collect_stream(bot.stream("search-tokenized-e2e", "帮我找靠窗座位资料")).await;
            assert!(events.iter().any(
                |event| matches!(event, CatEvent::Text(text) if text.contains("search observed"))
            ));
            let search_result = events
                .iter()
                .find_map(|event| match event {
                    CatEvent::ToolCallResult {
                        name,
                        result,
                        success,
                        ..
                    } if name == "search" && *success => Some(result),
                    _ => None,
                })
                .expect("search tool should return a successful result");
            let value: serde_json::Value =
                serde_json::from_str(search_result).expect("search result should be JSON");
            let results = value["results"]
                .as_array()
                .expect("search result should contain results");
            assert!(
                results.iter().any(|item| item["scope"] == "memory"
                    && item["snippet"]
                        .as_str()
                        .is_some_and(|snippet| snippet.contains("靠窗位置"))),
                "search result did not include tokenized memory match: {search_result}"
            );
            assert!(
                results
                    .iter()
                    .any(|item| item["scope"] == "skills" && item["name"] == "window-seat"),
                "search result did not include tokenized skill match: {search_result}"
            );

            let requests = requests.lock().expect("request lock poisoned");
            assert_eq!(requests.len(), 2);
            assert!(
                requests[1].contains("靠窗位置") && requests[1].contains("window-seat"),
                "second model request did not include local search tool results: {}",
                requests[1]
            );
        });
    }

    async fn collect_stream(stream: impl futures::Stream<Item = CatEvent>) -> Vec<CatEvent> {
        let mut stream = std::pin::pin!(stream);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            if let CatEvent::Error(err) = &event {
                panic!("stream error: {err}");
            }
            let done = matches!(event, CatEvent::Done);
            events.push(event);
            if done {
                break;
            }
        }
        events
    }

    fn tool_chain_messages(exchange: usize) -> Vec<Message> {
        use remi_agentloop::types::{FunctionCall, ToolCallMessage};
        let call_id = format!("call-{exchange}");
        let mut assistant = Message::assistant(format!("checking exchange {exchange}"));
        assistant.tool_calls = Some(vec![ToolCallMessage {
            id: call_id.clone(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "now".to_string(),
                arguments: format!(r#"{{"timezone":"UTC","exchange":{exchange}}}"#),
            },
        }]);
        vec![
            Message::user(format!("user exchange {exchange}")),
            assistant,
            Message::tool_result(call_id, format!("2026-07-14T00:00:{exchange:02}Z")),
            Message::assistant(format!("exchange {exchange} complete")),
        ]
    }

    #[test]
    fn long_tool_history_compacts_and_continues_end_to_end() {
        run_large_stack_local_test(|| async {
            std::env::set_var("OPENAI_API_KEY", "test");
            std::env::set_var("REMI_AUTO_COMPRESS_CONTEXT_PERCENT", "80");
            let (base_url, requests) = start_openai_mock_server(vec![
                sse_text("自由格式摘要：六个工具交换均已闭环，最后状态 exchange 6 complete。"),
                sse_text("continuity preserved after compaction"),
            ])
            .await;
            let data_dir = tempfile::tempdir().unwrap();
            let models_dir = data_dir.path().join("models");
            install_embedded_model_profiles(&models_dir).unwrap();
            let mut profile = test_model_profile();
            profile.base_url = Some(base_url);
            profile.model = "mock-model".to_string();
            profile.context_tokens = 128_000;
            profile.max_output_tokens = 4_096;
            let bot = CatBotBuilder {
                api_key: "test".to_string(),
                model_profile: profile,
                runtime_model_locked: false,
                system: default_system_prompt(),
                skills_dir: data_dir.path().join("skills"),
                data_dir: data_dir.path().to_path_buf(),
                agent_md_path: None,
                overflow_bytes: None,
                memory_days: 7,
                sandbox_config: SandboxConfig::Disabled {
                    host_dir: data_dir.path().to_path_buf(),
                },
                im_bridge: None,
                extra_options: serde_json::Map::new(),
                tool_allowlist: None,
                delegate_ids: Vec::new(),
                active_agent_id: DEFAULT_AGENT_ID.to_string(),
                model_bindings: AgentModelBindings::default(),
                approval_model_profile_id: None,
                agents_dir: data_dir.path().join("agents"),
                max_turns: Some(4),
                model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
                acp_client_tools: None,
            }
            .build()
            .unwrap();
            let thread = "tool-chain-20-plus-e2e";
            let run_lock = bot.thread_run_lock(thread).await;
            let run_guard = run_lock.lock().await;
            let busy_result =
                tokio::time::timeout(Duration::from_millis(100), bot.compact_memory(thread))
                    .await
                    .expect("busy /compact must not wait on the session lock")
                    .expect_err("busy /compact should return an explicit error");
            assert!(busy_result.to_string().contains("session is running"));
            drop(run_guard);
            let messages = (1..=6).flat_map(tool_chain_messages).collect::<Vec<_>>();
            assert_eq!(messages.len(), 24);
            bot.memory
                .append_failed_turn(thread, messages)
                .await
                .unwrap();
            assert_eq!(bot.compact_memory(thread).await.unwrap(), 16);
            let events = collect_stream(bot.stream(thread, "continue from exchange 6")).await;
            assert!(events.iter().any(
                |event| matches!(event, CatEvent::Text(text) if text.contains("continuity preserved"))
            ));
            assert_eq!(bot.memory.thread_history(thread).await.len(), 26);
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].contains("call-1") && requests[0].contains("2026-07-14T00:00:01Z"));
            assert!(requests[1].contains("LATEST COMPRESSED MEMORY"));
        });
    }

    #[test]
    fn full_request_budget_triggers_pre_request_compaction_end_to_end() {
        run_large_stack_local_test(|| async {
            std::env::set_var("OPENAI_API_KEY", "test");
            std::env::set_var("REMI_AUTO_COMPRESS_CONTEXT_PERCENT", "55");
            let (base_url, requests) = start_openai_mock_server(vec![
                sse_text("预算触发摘要：旧交换已压缩。"),
                sse_text("automatic compaction completed before this answer"),
            ])
            .await;
            let data_dir = tempfile::tempdir().unwrap();
            let models_dir = data_dir.path().join("models");
            install_embedded_model_profiles(&models_dir).unwrap();
            let mut profile = test_model_profile();
            profile.base_url = Some(base_url);
            profile.model = "mock-model".to_string();
            profile.context_tokens = 40_000;
            profile.max_output_tokens = 4_096;
            let bot = CatBotBuilder {
                api_key: "test".to_string(),
                model_profile: profile,
                runtime_model_locked: false,
                system: default_system_prompt(),
                skills_dir: data_dir.path().join("skills"),
                data_dir: data_dir.path().to_path_buf(),
                agent_md_path: None,
                overflow_bytes: None,
                memory_days: 7,
                sandbox_config: SandboxConfig::Disabled {
                    host_dir: data_dir.path().to_path_buf(),
                },
                im_bridge: None,
                extra_options: serde_json::Map::new(),
                tool_allowlist: None,
                delegate_ids: Vec::new(),
                active_agent_id: DEFAULT_AGENT_ID.to_string(),
                model_bindings: AgentModelBindings::default(),
                approval_model_profile_id: None,
                agents_dir: data_dir.path().join("agents"),
                max_turns: Some(4),
                model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
                acp_client_tools: None,
            }
            .build()
            .unwrap();
            let thread = "automatic-budget-compaction-e2e";
            let messages = (1..=8)
                .flat_map(|i| {
                    vec![
                        Message::user(format!("exchange {i}: {}", "context ".repeat(700))),
                        Message::assistant(format!("answer {i}: {}", "evidence ".repeat(500))),
                    ]
                })
                .collect::<Vec<_>>();
            bot.memory
                .append_failed_turn(thread, messages)
                .await
                .unwrap();
            let events = collect_stream(bot.stream(thread, "final continuity check")).await;
            assert!(events.iter().any(
                |event| matches!(event, CatEvent::Text(text) if text.contains("automatic compaction"))
            ));
            assert_eq!(bot.memory.thread_history(thread).await.len(), 18);
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                2,
                "compression must precede the main request"
            );
            assert!(!requests[0].contains("final continuity check"));
            assert!(requests[1].contains("final continuity check"));
            assert!(requests[1].contains("LATEST COMPRESSED MEMORY"));
        });
    }

    async fn start_openai_mock_server(
        responses: Vec<String>,
    ) -> (String, Arc<StdMutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        let responses = Arc::new(StdMutex::new(VecDeque::from(responses)));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&captured_requests);
                let responses = Arc::clone(&responses);
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let header_end = loop {
                        let mut chunk = [0_u8; 1024];
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = find_header_end(&buffer) {
                            break pos;
                        }
                    };
                    let headers = String::from_utf8_lossy(&buffer[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length:")
                                .or_else(|| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let body_start = header_end + 4;
                    while buffer.len() < body_start + content_length {
                        let mut chunk = vec![0_u8; body_start + content_length - buffer.len()];
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                    }
                    let body =
                        String::from_utf8_lossy(&buffer[body_start..body_start + content_length])
                            .to_string();
                    requests.lock().expect("request lock poisoned").push(body);
                    let response_body = responses
                        .lock()
                        .expect("response lock poisoned")
                        .pop_front()
                        .expect("missing mock response");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });
        (format!("http://{addr}/v1"), requests)
    }

    async fn start_slow_openai_mock_server(
        first_content: &str,
    ) -> (String, Arc<StdMutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        let first_content = first_content.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&captured_requests);
                let first_content = first_content.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let header_end = loop {
                        let mut chunk = [0_u8; 1024];
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = find_header_end(&buffer) {
                            break pos;
                        }
                    };
                    let headers = String::from_utf8_lossy(&buffer[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length:")
                                .or_else(|| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let body_start = header_end + 4;
                    while buffer.len() < body_start + content_length {
                        let mut chunk = vec![0_u8; body_start + content_length - buffer.len()];
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                    }
                    let body =
                        String::from_utf8_lossy(&buffer[body_start..body_start + content_length])
                            .to_string();
                    requests.lock().expect("request lock poisoned").push(body);

                    let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                    socket.write_all(headers.as_bytes()).await.unwrap();
                    let chunk = json!({
                        "choices": [{
                            "index": 0,
                            "delta": {"content": first_content},
                            "finish_reason": null
                        }]
                    });
                    socket
                        .write_all(format!("data: {chunk}\n\n").as_bytes())
                        .await
                        .unwrap();
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = socket.write_all(b"data: [DONE]\n\n").await;
                });
            }
        });
        (format!("http://{addr}/v1"), requests)
    }

    async fn start_openai_mock_server_with_slow_tail(
        responses: Vec<String>,
        slow_content: &str,
    ) -> (String, Arc<StdMutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        let responses = Arc::new(StdMutex::new(VecDeque::from(responses)));
        let slow_content = slow_content.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let requests = Arc::clone(&captured_requests);
                let responses = Arc::clone(&responses);
                let slow_content = slow_content.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let header_end = loop {
                        let mut chunk = [0_u8; 1024];
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = find_header_end(&buffer) {
                            break pos;
                        }
                    };
                    let headers = String::from_utf8_lossy(&buffer[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length:")
                                .or_else(|| line.strip_prefix("content-length:"))
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let body_start = header_end + 4;
                    while buffer.len() < body_start + content_length {
                        let mut chunk = vec![0_u8; body_start + content_length - buffer.len()];
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..n]);
                    }
                    let body =
                        String::from_utf8_lossy(&buffer[body_start..body_start + content_length])
                            .to_string();
                    requests.lock().expect("request lock poisoned").push(body);
                    let response_body = responses
                        .lock()
                        .expect("response lock poisoned")
                        .pop_front();
                    if let Some(response_body) = response_body {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            response_body.len(),
                            response_body
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                        return;
                    }

                    let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                    socket.write_all(headers.as_bytes()).await.unwrap();
                    let chunk = json!({
                        "choices": [{
                            "index": 0,
                            "delta": {"content": slow_content},
                            "finish_reason": null
                        }]
                    });
                    socket
                        .write_all(format!("data: {chunk}\n\n").as_bytes())
                        .await
                        .unwrap();
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let _ = socket.write_all(b"data: [DONE]\n\n").await;
                });
            }
        });
        (format!("http://{addr}/v1"), requests)
    }

    fn find_header_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn sse_text(content: &str) -> String {
        let chunk = json!({
            "choices": [{
                "index": 0,
                "delta": {"content": content},
                "finish_reason": null
            }]
        });
        format!("data: {}\n\ndata: [DONE]\n\n", chunk)
    }

    fn sse_tool_call(id: &str, name: &str, arguments: serde_json::Value) -> String {
        let chunk = json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments.to_string()
                        }
                    }]
                },
                "finish_reason": null
            }]
        });
        format!("data: {}\n\ndata: [DONE]\n\n", chunk)
    }

    #[tokio::test]
    async fn intermediate_state_updates_are_persisted_and_forwarded() {
        let data_dir =
            std::env::temp_dir().join(format!("remi-cat-state-update-{}", Uuid::new_v4()));
        let memory = super::MemoryStore {
            data_dir: data_dir.clone(),
            agent_md_path: None,
            compressor: LlmCompressor::new(
                "test-key".to_string(),
                None,
                "gpt-4o-mini".to_string(),
                128_000,
                4096,
                serde_json::Map::new(),
            ),
            short_term_tokens: 1024,
            auto_compress: true,
            memory_days: 7,
        };
        let user_state = json!({
            "__todos": [TodoItem {
                id: 6,
                content: "Summarize future trends".to_string(),
                description: None,
                done: true,
                batch_id: Some(1),
                batch_title: Some("Knowledge synthesis".to_string()),
                batch_index: Some(0),
            }]
        });

        let event =
            super::persist_intermediate_user_state(&memory, "thread-1", user_state.clone()).await;

        match event {
            super::CatEvent::StateUpdate(forwarded) => assert_eq!(forwarded, user_state),
            _ => panic!("expected StateUpdate event"),
        }

        let persisted = tokio::fs::read_to_string(data_dir.join("memory/thread-1/user_state.json"))
            .await
            .expect("intermediate user_state should be saved to disk");
        let persisted: serde_json::Value =
            serde_json::from_str(&persisted).expect("persisted user_state should deserialize");
        assert_eq!(persisted, user_state);

        let _ = tokio::fs::remove_dir_all(&data_dir).await;
    }
}

fn default_system_prompt() -> String {
    "You are a helpful AI assistant. \
     Use search for discovery and recall: by default scope=local searches local memory and skills; \
     set scope=memory for named/thread memory, scope=skills for skill catalog lookup, \
     scope=web for Exa web search, or scope=all for every available source. \
     If the user asks about their own prior facts, preferences, past actions, earlier conversations, \
     saved details, or anything phrased like \"my ...\", you MUST call search with scope=memory \
     and distinctive keywords from the question before giving the final answer. Do not say prior \
     information is unavailable until you have searched memory. \
     Pinned skills are only a curated subset of available skills. More relevant skills may exist \
     beyond the pinned list; use search to discover skill catalog entries and saved memory. Before \
     starting substantive work, search for relevant skills and memory so the answer is informed by \
     reusable procedures and prior context instead of relying only on the visible pinned skills or \
     the current conversation. \
     If the user asks you to remember, store, keep, or use a long note, transcript, document excerpt, \
     or historical conversation as future context, use memory__upsert_named to save a concise but \
     complete named memory before acknowledging it; do not rely only on short-term chat history. \
     When editing files, prefer apply_patch for focused changes to existing files. Use fs_write \
     only to create a new file or when you intentionally replace a whole file after reading it. \
    Use skill__get to inspect reusable procedures; use fs_read with the returned resource_root_path for related resources. Use todo tools \
     (todo__add/list/complete/update/remove) to track multi-step work per thread; \
     todo__add creates a titled batch of child todos and returns their IDs, \
     memory__upsert_named to save stable facts, preferences, and project conventions, \
     and memory__get_detail to read full compressed memory blocks. \
     Use them when appropriate."
        .to_string()
}
