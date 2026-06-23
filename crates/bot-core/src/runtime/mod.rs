use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_agentloop::agent_loop::AgentLoop;
use remi_agentloop::prelude::{
    AgentBuilder, AgentConfig, AgentError, LoopInput, MessageId, OpenAIClient, ReqwestTransport,
    ResumePayload, Role, RunId, SubSessionEvent, SubSessionEventPayload, ThreadId, Tool,
    ToolContext, ToolOutput, ToolResult,
};
use remi_agentloop::tool::registry::DefaultToolRegistry;
use tokio::sync::Mutex as AsyncMutex;

use crate::hooks::HookContext;
use crate::im_tools::register_fetch_tool;
use crate::memory::{
    build_injected_history, LlmCompressor, MemoryGetDetailTool, MemoryRecallTool,
    MemoryUpsertNamedTool,
};
use crate::sandbox::SandboxConfig;
use crate::search::SearchTool;
use crate::skill::store::SkillStore;
use crate::tools::{
    BashMode, ExaSearchTool, ManageYourselfTool, NowTool, RootedFsApplyPatchTool,
    RootedFsCreateTool, RootedFsLsTool, RootedFsReadTool, RootedFsRemoveTool, RootedFsWriteTool,
    SecretRedactor, SleepTool, WorkspaceBashTool, WorkspaceSshTool,
};
use crate::{
    acp, context_budget_tokens, embedded_agent_profile, goal, install_embedded_agent_profiles,
    install_embedded_model_profiles, model_usage, remi_skill, resolve_model_profile_from_env,
    sandbox, skill, supervisor_workflow, todo, trigger, AccountUsage, AgentModelBindings,
    AgentProfile, AgentRegistry, AskUserQuestionTool, BuiltinSkillStore, CatAgent, CatEvent,
    Content, ContentPart, FileSkillStore, GoalMaxRounds, GoalState, HookEventName, HookManager,
    ImAttachment, ImDocument, ImFileBridge, MemoryStore, Message, ModelProfileConfig,
    ModelProfileRegistry, SharedRedactor, SkillDocument, SkillLoadDiagnostic, SkillSummary,
    SupervisorTraceEvent, ThreadHistoryMessage, ToolApprovalManager, UserQuestionManager,
    WorkflowDecision, WorkflowDefinition, WorkflowInstance, WorkflowMaxRounds, WorkflowReport,
    WorkflowStatus,
};

mod approval_markers;
mod partial_turn;
mod prompting;
mod tool_status;
pub(crate) use approval_markers::{
    cat_event_from_subagent_approval_marker, subagent_approval_marker,
    tool_approval_requested_marker, tool_approval_resolved_marker, tool_approval_updated_marker,
    user_question_requested_marker, user_question_resolved_marker, user_question_updated_marker,
};
pub(crate) use partial_turn::PartialTurnRecorder;
pub(crate) use prompting::{
    append_thread_todo_system_prompt, apply_skill_injections,
    insert_single_chat_sender_system_prompt, insert_skill_injection_prompts, is_direct_chat,
    model_input_snapshot_from_loop_input, prepend_group_sender_username, route_thread_todo_prompt,
    single_chat_sender_system_prompt, truncate_user_name,
};
pub(crate) use tool_status::{
    builtin_tool_catalog, tool_errors, tool_runtime_errors, tool_warnings,
};

const DEFAULT_AGENT_ID: &str = "default";
const SUPERVISOR_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT: usize = 80;
pub(crate) const TRIGGER_RUN_META_KEY: &str = "trigger_run";

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

pub(crate) fn suppress_trigger_management(metadata: Option<&serde_json::Value>) -> bool {
    metadata_flag_enabled(metadata, TRIGGER_RUN_META_KEY)
}

// -- StreamOptions ----------------------------------------------------------

/// Per-turn options for [`CatBot::stream_with_options`].
#[derive(Debug, Default, Clone)]
pub struct StreamOptions {
    /// Optional session-persisted model profile override.
    pub model_profile_id: Option<String>,
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
    /// Route newly-created todo batches to the remi-sdk backend when true.
    pub todo_create_via_sdk: bool,
    /// Enable trigger management tools for the current turn.
    pub trigger_tools_enabled: bool,
    /// Marks the request as an automatic trigger execution.
    /// Trigger management tools and the builtin trigger skill are hidden for these runs.
    pub trigger_run: bool,
    /// Marks the request as an internal supervisor run.
    /// Goal supervision is disabled for these runs to avoid recursive loops.
    pub supervisor_run: bool,
    /// Downloadable IM attachments referenced by the current message.
    pub im_attachments: Vec<ImAttachment>,
    /// Feishu document links referenced by the current message.
    pub im_documents: Vec<ImDocument>,
    /// Optional cooperative-cancel signal.  When the wrapped [`Notify`] is
    /// signalled (via `notify_one()`), `stream_with_options` will persist any
    /// already-generated content and yield a final [`CatEvent::Done`] before
    /// returning — so memory is not lost on preemption.
    pub cancel: Option<std::sync::Arc<tokio::sync::Notify>>,
}

// -- Type aliases -------------------------------------------------------------

type InnerAgent = AgentLoop<OpenAIClient<ReqwestTransport>>;
type ThreadRunLock = Arc<AsyncMutex<()>>;
type ThreadRunLocks = Arc<AsyncMutex<HashMap<String, ThreadRunLock>>>;

struct LocalAcpAgentRunner {
    agent: CatAgent<InnerAgent>,
    memory: Arc<MemoryStore>,
    run_locks: ThreadRunLocks,
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
            let mut history = build_injected_history(&ctx);
            append_thread_todo_system_prompt(&mut history, &ctx.user_state);
            let skip_count = history.len();
            let input = LoopInput::start(message)
                .history(history)
                .metadata(serde_json::json!({ "thread_id": &thread_id }))
                .user_state(std::mem::take(&mut ctx.user_state));
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
    skill_store: Arc<BuiltinSkillStore<FileSkillStore>>,
    memory: Arc<MemoryStore>,
    todo_backend: Arc<todo::HybridTodoBackend>,
    trigger_backend: Arc<trigger::TriggerBackend>,
    acp_backend: Arc<acp::AcpBackend>,
    approval_manager: Arc<ToolApprovalManager>,
    user_question_manager: Arc<UserQuestionManager>,
    hook_manager: Arc<HookManager>,
    run_locks: ThreadRunLocks,
    model_profile: ModelProfileConfig,
    model_registry: Arc<ModelProfileRegistry>,
    api_key: String,
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
pub struct EffectiveModelProfile {
    pub profile: ModelProfileConfig,
    pub source: EffectiveModelSource,
    pub invalid_session_model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveModelSource {
    Session,
    Default,
}

impl CatBot {
    pub fn model_context_tokens(&self) -> u32 {
        self.model_profile.context_tokens
    }

    pub fn approval_manager(&self) -> Arc<ToolApprovalManager> {
        Arc::clone(&self.approval_manager)
    }

    pub fn user_question_manager(&self) -> Arc<UserQuestionManager> {
        Arc::clone(&self.user_question_manager)
    }

    pub fn hook_manager(&self) -> Arc<HookManager> {
        Arc::clone(&self.hook_manager)
    }

    pub fn model_context_tokens_for(&self, session_model_profile_id: Option<&str>) -> u32 {
        self.effective_model_profile(session_model_profile_id)
            .profile
            .context_tokens
    }

    pub fn model_profiles(&self) -> Vec<&ModelProfileConfig> {
        self.model_registry.list()
    }

    pub fn skill_summaries(&self) -> Vec<SkillSummary> {
        self.skill_store.featured_summaries()
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
        if let Some(id) = session_model_profile_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Some(profile) = self.model_registry.get(id) {
                return EffectiveModelProfile {
                    profile: profile.clone(),
                    source: EffectiveModelSource::Session,
                    invalid_session_model: None,
                };
            }
            return EffectiveModelProfile {
                profile: self.model_profile.clone(),
                source: EffectiveModelSource::Default,
                invalid_session_model: Some(id.to_string()),
            };
        }
        EffectiveModelProfile {
            profile: self.model_profile.clone(),
            source: EffectiveModelSource::Default,
            invalid_session_model: None,
        }
    }

    pub async fn account_usage(&self) -> anyhow::Result<AccountUsage> {
        model_usage::query_account_usage(&self.model_profile, &self.api_key).await
    }

    pub async fn account_usage_for(
        &self,
        session_model_profile_id: Option<&str>,
    ) -> anyhow::Result<AccountUsage> {
        let effective = self.effective_model_profile(session_model_profile_id);
        model_usage::query_account_usage(&effective.profile, &self.api_key).await
    }

    fn agent_for_model_profile(&self, profile_id: &str) -> &CatAgent<InnerAgent> {
        if profile_id == self.model_profile.id {
            &self.inner
        } else {
            self.model_agents.get(profile_id).unwrap_or(&self.inner)
        }
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
    /// | `OPENAI_API_KEY`          | API key (required)                                    |
    /// | `REMI_MODEL_PROFILE`      | Selected model profile id (default: `default`)        |
    /// | `REMI_KIMI_THINKING`      | Optional thinking override for supported Kimi profiles |
    /// | `REMI_MEMORY_DAYS`        | Days before mid-term → long-term (default: 7)         |
    /// | `REMI_AUTO_COMPRESS_CONTEXT_PERCENT` | Context usage percent that triggers auto compression (default: 80) |
    /// | `REMI_SHORT_TERM_TOKENS`  | Absolute auto-compression token threshold override    |
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
        let _run_guard = run_lock.lock().await;
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
        self.trigger_backend
            .delete_thread(thread_id, user_id)
            .await
            .map_err(|err| AgentError::other(format!("delete thread triggers: {err:#}")))?;
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
        self.todo_backend
            .fork_thread_user_state(source_thread_id, target_thread_id, user_id, &mut user_state)
            .await
            .map_err(|err| AgentError::other(format!("fork thread todos: {err:#}")))?;
        self.trigger_backend
            .fork_thread_user_state(source_thread_id, target_thread_id, user_id, &mut user_state)
            .await
            .map_err(|err| AgentError::other(format!("fork thread triggers: {err:#}")))?;
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
        instance.updated_at = chrono::Utc::now();
        instance.last_report = Some(WorkflowReport {
            workflow_id: instance.definition.id.clone(),
            workflow_name: instance.definition.name.clone(),
            from_node: instance.current_node.clone(),
            edge: None,
            to_node: instance.current_node.clone(),
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
        let running = lock.try_lock().is_err();
        running
    }

    /// List triggers for the current thread without invoking the LLM.
    pub async fn trigger_list_command(
        &self,
        thread_id: &str,
        opts: StreamOptions,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        let ctx = trigger_command_tool_ctx(thread_id, &opts);
        let items = self.trigger_backend.list(&ctx).await?;
        if items.is_empty() {
            Ok("当前线程没有触发器。".to_string())
        } else {
            Ok(format!(
                "**当前线程的触发器：**\n\n{}",
                crate::trigger::tools::format_trigger_list(&items)
            ))
        }
    }

    /// Delete one trigger for the current thread without invoking the LLM.
    pub async fn trigger_delete_command(
        &self,
        thread_id: &str,
        id: u64,
        opts: StreamOptions,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        let ctx = trigger_command_tool_ctx(thread_id, &opts);
        let result = self.trigger_backend.delete(&ctx, id).await?;
        if result.starts_with("Removed trigger #") {
            Ok(format!("✅ 已删除触发器 #{id}。"))
        } else if result.contains("not found") {
            Ok(format!("❌ 触发器 #{id} 不存在。"))
        } else {
            Ok(result)
        }
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

    /// Create or update one trigger from a JSON payload without invoking the LLM.
    pub async fn trigger_upsert_command(
        &self,
        thread_id: &str,
        arguments_json: &str,
        opts: StreamOptions,
    ) -> Result<String, remi_agentloop::prelude::AgentError> {
        let args = serde_json::from_str(arguments_json).map_err(|err| {
            remi_agentloop::prelude::AgentError::tool(
                "trigger",
                format!("invalid trigger JSON: {err}"),
            )
        })?;
        let request = crate::trigger::tools::parse_upsert_request(args)?;
        let ctx = trigger_command_tool_ctx(thread_id, &opts);
        let result = self.trigger_backend.upsert(&ctx, request).await?;
        let action = if result.operation == "updated" {
            "已更新"
        } else {
            "已创建"
        };
        Ok(format!(
            "✅ {action}触发器。\n\n{}",
            crate::trigger::tools::format_trigger_item(&result.item)
        ))
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
        use remi_agentloop::tool::registry::ToolRegistry;
        self.inner
            .local_tools
            .definitions(&serde_json::Value::Null)
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

    /// Return every tool known to this runtime, without applying the active
    /// agent allowlist. This is intended for diagnostics and self-management:
    /// an agent can inspect unavailable tools and decide how to update its
    /// profile allowlist or runtime configuration.
    pub fn registered_tool_statuses(&self) -> Vec<RegisteredToolStatus> {
        use remi_agentloop::tool::registry::ToolRegistry;

        let enabled_defs = self.inner.local_tools.definitions(&serde_json::Value::Null);
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
            let registered = self.inner.local_tools.contains(name);
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

        if !self.acp_backend.codex_tool_available() {
            by_name
                .entry("codex".to_string())
                .and_modify(|entry| {
                    entry.registered = false;
                    entry.enabled = false;
                    entry.errors = tool_errors("codex", false);
                })
                .or_insert_with(|| RegisteredToolStatus {
                    name: "codex".to_string(),
                    description: "Open, resume, or poll a Codex ACP session.".to_string(),
                    registered: false,
                    enabled: false,
                    in_active_allowlist: self.tool_in_active_allowlist("codex"),
                    warnings: Vec::new(),
                    errors: tool_errors("codex", false),
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
                progress,
            )
            .await
        {
            Ok(decision) => decision,
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
        progress: tokio::sync::mpsc::UnboundedSender<SupervisorTraceEvent>,
    ) -> Result<WorkflowDecision, AgentError> {
        let supervisor_thread_id = format!("supervisor:{thread_id}:{}", uuid::Uuid::new_v4());
        let prompt = supervisor_workflow::supervisor_prompt(instance, history, todo_prompt)
            .map_err(AgentError::other)?;
        let input = LoopInput::start(prompt).metadata(serde_json::json!({
            "thread_id": supervisor_thread_id,
            "supervisor_run": "true",
        }));
        let effective_model = self.effective_model_profile(model_profile_id);
        let active_agent = self.agent_for_model_profile(&effective_model.profile.id);
        let output = tokio::time::timeout(SUPERVISOR_TIMEOUT, async {
            let mut stream = std::pin::pin!(active_agent.stream_with_input(input));
            let mut output = String::new();
            let mut trace = Vec::new();
            while let Some(event) = stream.next().await {
                match event {
                    CatEvent::Text(delta) => {
                        output.push_str(&delta);
                        let _ = progress.send(SupervisorTraceEvent::OutputDelta { content: delta });
                    }
                    CatEvent::Thinking(content) => {
                        let event = SupervisorTraceEvent::Thinking { content };
                        let _ = progress.send(event.clone());
                        trace.push(event);
                    }
                    CatEvent::ToolCall { name, args, .. } => {
                        let event = SupervisorTraceEvent::ToolCall { name, args };
                        let _ = progress.send(event.clone());
                        trace.push(event);
                    }
                    CatEvent::ToolCallResult { name, result, .. } => {
                        let event = SupervisorTraceEvent::ToolResult { name, result };
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
            trace.push(SupervisorTraceEvent::Output {
                content: output.clone(),
            });
            Ok((output, trace))
        })
        .await
        .map_err(|_| AgentError::other("supervisor evaluation timed out after 180 seconds"))??;
        let (output, trace) = output;
        let mut decision =
            supervisor_workflow::parse_decision(&output).map_err(AgentError::other)?;
        decision.trace = trace;
        Ok(decision)
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
            let effective_model = self.effective_model_profile(opts.model_profile_id.as_deref());
            let active_agent = self.agent_for_model_profile(&effective_model.profile.id);
            let mut next_content = content;
            let mut supervisor_round: u32 = 0;
            let mut continuation_from_supervisor = false;
            let mut stop_hook_active = false;
            let turn_started = Instant::now();
            tracing::info!(
                thread_id = %thread_id_owned,
                model_profile = %effective_model.profile.id,
                model = %effective_model.profile.model,
                model_source = ?effective_model.source,
                platform = opts.platform.as_deref().unwrap_or(""),
                chat_type = opts.chat_type.as_deref().unwrap_or(""),
                message_id = opts.message_id.as_deref().unwrap_or(""),
                sender_user_id = opts.sender_user_id.as_deref().unwrap_or(""),
                supervisor_run = opts.supervisor_run,
                trigger_run = opts.trigger_run,
                "agent_turn.start"
            );

            'workflow_loop: loop {
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
                    "failed to refresh sdk-backed todo state before turn"
                );
            }

            if let Err(err) = self
                .trigger_backend
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
                    "failed to refresh sdk-backed trigger state before turn"
                );
            }

            let mut round_opts = opts.clone();
            if continuation_from_supervisor {
                round_opts.sender_username = Some("supervisor".to_string());
                round_opts.sender_user_id = Some("supervisor".to_string());
                round_opts.message_id = None;
                round_opts.im_attachments.clear();
                round_opts.im_documents.clear();
            }

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
            let agent_header_count =
                usize::from(ctx.agent_md.is_some()) + usize::from(ctx.soul_md.is_some());
            insert_skill_injection_prompts(
                &mut history,
                agent_header_count,
                &round_opts.skill_injections,
            );
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
            let skip_count;

            // 3. Build request-level metadata (thread_id for tools);
            //    build per-message metadata (sender identity + message id).
            let mut meta = serde_json::json!({ "thread_id": &thread_id_owned });
            if let Some(ref ct) = round_opts.chat_type {
                meta["chat_type"] = serde_json::Value::String(ct.clone());
            }
            if let Some(ref platform) = round_opts.platform {
                meta["platform"] = serde_json::Value::String(platform.clone());
            }
            if round_opts.todo_create_via_sdk {
                meta["todo_create_via_sdk"] = serde_json::Value::String("true".to_string());
            }
            if round_opts.trigger_tools_enabled {
                meta["trigger_tools_enabled"] = serde_json::Value::String("true".to_string());
            }
            if round_opts.trigger_run {
                meta[TRIGGER_RUN_META_KEY] = serde_json::Value::String("true".to_string());
            }
            if round_opts.supervisor_run {
                meta["supervisor_run"] = serde_json::Value::String("true".to_string());
            }

            let mut msg_meta = serde_json::Map::new();
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
            if !continuation_from_supervisor {
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
            partial_base_history.push(current_user_message);
            let mut partial_turn = PartialTurnRecorder::new(partial_base_history);

            let mut input = LoopInput::start_content(content)
                .history(history)
                .metadata(meta)
                .user_state(ctx.user_state);
            if let Some(user_name) = injected_user_name {
                input = input.user_name(user_name);
            }
            if let Some(mm) = message_metadata {
                input = input.message_metadata(mm);
            }

            if let Some(snapshot) = model_input_snapshot_from_loop_input(
                &input,
                &thread_id_owned,
                round_opts.message_id.as_deref(),
                &effective_model.profile.id,
                &effective_model.profile.model,
            ) {
                yield CatEvent::ModelInputSnapshot(snapshot);
            }

            yield CatEvent::StateUpdate(initial_user_state.clone());

            // 4. Drive inner agent, intercept History event to persist.
            let mut raw_history: Option<Vec<Message>> = None;
            let mut raw_user_state: Option<serde_json::Value> = None;
            let mut tool_elapsed_ms = HashMap::<String, u64>::new();
            let cancel = round_opts.cancel.clone();
            let inner_stream = active_agent.stream_with_input(input);
            let mut inner_stream = std::pin::pin!(inner_stream);

            loop {
                // When a cancel notify is present, race the inner stream against
                // the cancel signal so in-progress content is persisted even when
                // the task is preempted (e.g. by a newer incoming message).
                enum SelectOut {
                    Event(Option<CatEvent>),
                    Cancelled,
                }
                let outcome = if let Some(ref notify) = cancel {
                    tokio::select! {
                        ev = inner_stream.next() => SelectOut::Event(ev),
                        _ = notify.notified() => SelectOut::Cancelled,
                    }
                } else {
                    SelectOut::Event(inner_stream.next().await)
                };

                match outcome {
                    SelectOut::Cancelled => {
                        tracing::info!(
                            thread_id = %thread_id_owned,
                            model_profile = %effective_model.profile.id,
                            elapsed_ms = turn_started.elapsed().as_millis() as u64,
                            "agent_turn.cancelled"
                        );
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
                    SelectOut::Event(None) => break,
                    SelectOut::Event(Some(ev)) => match ev {
                        CatEvent::History(msgs, us) => {
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
                        CatEvent::StateUpdate(us) => {
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
                            yield CatEvent::UserQuestionRequested(request);
                        }
                        CatEvent::UserQuestionUpdated(request) => {
                            yield CatEvent::UserQuestionUpdated(request);
                        }
                        CatEvent::UserQuestionResolved { request, response } => {
                            yield CatEvent::UserQuestionResolved { request, response };
                        }
                        // Save memory BEFORE yielding Done/Error — the caller drops
                        // the stream immediately on these events, so any code after
                        // this loop would never execute.
                        CatEvent::Done => {
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
                            if !round_opts.supervisor_run {
                                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
                                let evaluation = self.evaluate_workflow_after_round(
                                    &thread_id_owned,
                                    supervisor_history.as_deref().unwrap_or(&[]),
                                    supervisor_todo_prompt,
                                    supervisor_round,
                                    round_opts.model_profile_id.as_deref(),
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
                    },
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

enum WorkflowRoundOutcome {
    NoWorkflow,
    Report(WorkflowReport),
    Continue {
        report: WorkflowReport,
        message: String,
    },
}

fn workflow_round_allows_continue(
    max_rounds: &WorkflowMaxRounds,
    completed_continuations: u32,
) -> bool {
    match max_rounds {
        GoalMaxRounds::Limited(max) => completed_continuations < *max,
        GoalMaxRounds::Unlimited => true,
    }
}

fn hook_context_message(event: &str, content: String) -> Message {
    Message {
        id: MessageId::new(),
        role: Role::System,
        content: Content::text(format!(
            "Hook {event} provided additional context:\n{content}"
        )),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        reasoning_content: None,
        metadata: None,
    }
}

// -- Helpers ------------------------------------------------------------------

/// Save new turn messages and user_state to the memory store.
///
/// Strips the first `skip_count` messages (the injected history prefix) from
/// `history` before persisting, so only the new user + assistant messages are
/// appended to short-term storage.
async fn persist_turn(
    memory: &MemoryStore,
    thread_id: &str,
    history: Option<Vec<Message>>,
    user_state: Option<serde_json::Value>,
    skip_count: usize,
    tool_elapsed_ms: &HashMap<String, u64>,
) -> Vec<CatEvent> {
    let mut events = Vec::new();
    if let Some(all_msgs) = history {
        let mut new_msgs: Vec<Message> = all_msgs.into_iter().skip(skip_count).collect();
        annotate_tool_elapsed_ms(&mut new_msgs, tool_elapsed_ms);
        tracing::debug!(
            thread_id,
            skip_count,
            total_msgs = new_msgs.len(),
            msgs_with_metadata = new_msgs.iter().filter(|m| m.metadata.is_some()).count(),
            "persist_turn: saving messages"
        );
        for (i, m) in new_msgs.iter().enumerate() {
            tracing::debug!(
                i, role = ?m.role, has_metadata = m.metadata.is_some(), metadata = ?m.metadata,
                "persist_turn: message[{}]", i
            );
        }
        if !new_msgs.is_empty() {
            let mut sink = |event| events.push(CatEvent::ContextCompaction(event));
            if let Err(e) = memory
                .save_turn_with_compaction_events(thread_id, new_msgs, Some(&mut sink))
                .await
            {
                tracing::warn!(thread_id, error = %e, "memory.persist.failed");
            }
        }
    }
    if let Some(us) = user_state {
        if let Err(e) = memory.save_user_state(thread_id, &us).await {
            tracing::warn!(thread_id, error = %e, "memory.user_state.persist.failed");
        }
    }
    events
}

fn annotate_tool_elapsed_ms(messages: &mut [Message], tool_elapsed_ms: &HashMap<String, u64>) {
    for message in messages {
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let Some(elapsed_ms) = tool_elapsed_ms.get(call_id) else {
            continue;
        };
        let metadata = message
            .metadata
            .get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if let serde_json::Value::Object(map) = metadata {
            map.insert(
                "tool_elapsed_ms".to_string(),
                serde_json::Value::Number(serde_json::Number::from(*elapsed_ms)),
            );
        }
    }
}

async fn persist_intermediate_user_state(
    memory: &MemoryStore,
    thread_id: &str,
    user_state: serde_json::Value,
) -> CatEvent {
    if let Err(e) = memory.save_user_state(thread_id, &user_state).await {
        tracing::warn!(
            thread_id,
            intermediate = true,
            error = %e,
            "memory.user_state.persist.failed"
        );
    }
    CatEvent::StateUpdate(user_state)
}

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

fn trigger_command_tool_ctx(thread_id: &str, opts: &StreamOptions) -> ToolContext {
    ToolContext {
        config: AgentConfig::default(),
        thread_id: Some(
            serde_json::from_value(serde_json::json!(thread_id))
                .expect("thread_id should deserialize"),
        ),
        run_id: serde_json::from_value(serde_json::json!(format!("trigger-command:{thread_id}")))
            .expect("run_id should deserialize"),
        metadata: Some(trigger_command_metadata(thread_id, opts)),
        user_state: Arc::new(RwLock::new(serde_json::Value::Null)),
    }
}

fn trigger_command_metadata(thread_id: &str, opts: &StreamOptions) -> serde_json::Value {
    let mut meta = serde_json::json!({ "thread_id": thread_id });
    if let Some(ref sender_user_id) = opts.sender_user_id {
        meta["sender_user_id"] = serde_json::Value::String(sender_user_id.clone());
    }
    if let Some(ref sender_username) = opts.sender_username {
        meta["sender_username"] = serde_json::Value::String(sender_username.clone());
    }
    if let Some(ref message_id) = opts.message_id {
        meta["message_id"] = serde_json::Value::String(message_id.clone());
    }
    if let Some(ref chat_type) = opts.chat_type {
        meta["chat_type"] = serde_json::Value::String(chat_type.clone());
    }
    if let Some(ref platform) = opts.platform {
        meta["platform"] = serde_json::Value::String(platform.clone());
    }
    if opts.trigger_tools_enabled {
        meta["trigger_tools_enabled"] = serde_json::Value::String("true".to_string());
    }
    meta
}

struct LocalToolDeps {
    skill_store: Arc<BuiltinSkillStore<FileSkillStore>>,
    memory: Arc<MemoryStore>,
    todo_backend: Arc<todo::HybridTodoBackend>,
    trigger_backend: Arc<trigger::TriggerBackend>,
    acp_backend: Arc<acp::AcpBackend>,
    sandbox: Arc<dyn sandbox::Sandbox>,
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
    user_question_manager: Arc<UserQuestionManager>,
    hook_manager: Arc<HookManager>,
    overflow_bytes: usize,
}

impl LocalToolDeps {
    fn clone_for_subagent(&self) -> Self {
        Self {
            skill_store: Arc::clone(&self.skill_store),
            memory: Arc::clone(&self.memory),
            todo_backend: Arc::clone(&self.todo_backend),
            trigger_backend: Arc::clone(&self.trigger_backend),
            acp_backend: Arc::clone(&self.acp_backend),
            sandbox: Arc::clone(&self.sandbox),
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
            user_question_manager: Arc::clone(&self.user_question_manager),
            hook_manager: Arc::clone(&self.hook_manager),
            overflow_bytes: self.overflow_bytes,
        }
    }

    fn build_tools(
        &self,
        profile: &ModelProfileConfig,
        extra_options: serde_json::Map<String, serde_json::Value>,
        include_acp: bool,
    ) -> DefaultToolRegistry {
        let mut local_tools = DefaultToolRegistry::new();
        skill::register_skill_tools(&mut local_tools, Arc::clone(&self.skill_store));
        todo::register_todo_tools(&mut local_tools, Arc::clone(&self.todo_backend));
        trigger::register_trigger_tools(&mut local_tools, Arc::clone(&self.trigger_backend));
        if include_acp {
            acp::register_acp_tools(
                &mut local_tools,
                Arc::clone(&self.acp_backend),
                Arc::clone(&self.approval_manager),
            );
        }
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
        local_tools.register(MemoryGetDetailTool {
            store: Arc::clone(&self.memory),
            agent_id: self.active_agent_id.clone(),
        });
        local_tools.register(MemoryUpsertNamedTool {
            store: Arc::clone(&self.memory),
            agent_id: self.active_agent_id.clone(),
            workspace_root: self.workspace_root.clone(),
        });
        local_tools.register(MemoryRecallTool {
            store: Arc::clone(&self.memory),
            agent_id: self.active_agent_id.clone(),
        });
        local_tools.register(SearchTool {
            skill_store: Arc::clone(&self.skill_store),
            memory_store: Arc::clone(&self.memory),
            agent_id: self.active_agent_id.clone(),
        });
        if self.bash_enabled {
            local_tools.register(WorkspaceBashTool::new(
                Arc::clone(&self.sandbox),
                Arc::clone(&self.redactor),
            ));
        }
        local_tools.register(WorkspaceSshTool::new(Arc::clone(&self.redactor)));
        local_tools.register(RootedFsReadTool {
            sandbox: Arc::clone(&self.sandbox),
            redactor: Arc::clone(&self.redactor),
        });
        local_tools.register(RootedFsWriteTool {
            sandbox: Arc::clone(&self.sandbox),
        });
        local_tools.register(RootedFsApplyPatchTool {
            sandbox: Arc::clone(&self.sandbox),
        });
        local_tools.register(RootedFsCreateTool {
            sandbox: Arc::clone(&self.sandbox),
        });
        local_tools.register(RootedFsRemoveTool {
            sandbox: Arc::clone(&self.sandbox),
        });
        local_tools.register(RootedFsLsTool {
            sandbox: Arc::clone(&self.sandbox),
            redactor: Arc::clone(&self.redactor),
        });
        register_fetch_tool(
            &mut local_tools,
            self.workspace_root.clone(),
            self.sandbox.workspace_root_label(),
            self.sandbox.kind() != "docker",
            self.im_bridge.clone(),
        );
        local_tools.register(ExaSearchTool::new());
        local_tools.register(NowTool);
        local_tools.register(SleepTool);
        local_tools.register(ManageYourselfTool);
        local_tools.register(AskUserQuestionTool::new(Arc::clone(
            &self.user_question_manager,
        )));
        local_tools
    }
}

fn build_inner_agent(
    api_key: &str,
    profile: &ModelProfileConfig,
    system_prompt: String,
    max_turns: Option<usize>,
    extra_options: serde_json::Map<String, serde_json::Value>,
) -> InnerAgent {
    let mut model = OpenAIClient::new(api_key.to_string()).with_model(profile.model.clone());
    if let Some(url) = profile.base_url.clone() {
        model = model.with_base_url(url);
    }
    let mut builder = AgentBuilder::new()
        .model(model)
        .config(AgentConfig::default().with_max_tokens(profile.max_output_tokens))
        .system(system_prompt)
        .max_turns(max_turns.unwrap_or(usize::MAX));
    if !extra_options.is_empty() {
        builder = builder.extra_options(extra_options);
    }
    builder.build_loop()
}

fn auto_compress_context_percent() -> anyhow::Result<usize> {
    let Some(raw) = std::env::var("REMI_AUTO_COMPRESS_CONTEXT_PERCENT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return Ok(DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT);
    };
    let percent = raw.parse::<usize>().map_err(|err| {
        anyhow::anyhow!("invalid REMI_AUTO_COMPRESS_CONTEXT_PERCENT `{raw}`: {err}")
    })?;
    if !(1..=100).contains(&percent) {
        anyhow::bail!("REMI_AUTO_COMPRESS_CONTEXT_PERCENT must be between 1 and 100");
    }
    Ok(percent)
}

fn tool_output_overflow_bytes_from_env() -> anyhow::Result<Option<usize>> {
    let Some((key, raw)) = ["REMI_TOOL_OUTPUT_OVERFLOW_BYTES", "REMI_OVERFLOW_BYTES"]
        .into_iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| (key, value))
        })
    else {
        return Ok(None);
    };
    let bytes = raw
        .parse::<usize>()
        .map_err(|err| anyhow::anyhow!("invalid {key} `{raw}`: {err}"))?;
    if bytes == 0 {
        anyhow::bail!("{key} must be greater than 0");
    }
    Ok(Some(bytes))
}

fn context_percent_tokens(profile: &ModelProfileConfig, percent: usize) -> usize {
    context_budget_tokens(profile.context_tokens, percent)
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
    short_term_tokens: Option<usize>,
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
    agents_dir: PathBuf,
    max_turns: Option<usize>,
    model_registry: Arc<ModelProfileRegistry>,
}

impl CatBotBuilder {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("REMI_API_KEY"))
            .map_err(|_| anyhow::anyhow!("OPENAI_API_KEY or REMI_API_KEY must be set"))?;
        let memory_days = std::env::var("REMI_MEMORY_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7_u64);
        let short_term_tokens = std::env::var("REMI_SHORT_TERM_TOKENS")
            .ok()
            .and_then(|value| value.parse().ok());
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
            short_term_tokens,
            overflow_bytes,
            memory_days,
            sandbox_config,
            im_bridge: None,
            extra_options: resolved_model.extra_options,
            tool_allowlist: None,
            delegate_ids: Vec::new(),
            active_agent_id: DEFAULT_AGENT_ID.to_string(),
            model_bindings: AgentModelBindings::default(),
            agents_dir: data_dir.join("agents"),
            max_turns: None,
            model_registry,
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

    /// Override the short-term memory token budget.
    /// By default this is derived from the model profile context window and
    /// `REMI_AUTO_COMPRESS_CONTEXT_PERCENT` (80% when unset).
    pub fn short_term_tokens(mut self, n: usize) -> Self {
        self.short_term_tokens = Some(n);
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
        let short_term_tokens = self
            .short_term_tokens
            .unwrap_or_else(|| context_percent_tokens(&profile, auto_compress_context_percent));
        let overflow_bytes = self.overflow_bytes.unwrap_or(profile.overflow_bytes);
        let resolved_base_url = profile.base_url.clone();

        tracing::debug!(
            model = %profile.model,
            profile = %profile.id,
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
        let system_prompt = self.system.clone();

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
        let skill_store = Arc::new(BuiltinSkillStore::new(
            FileSkillStore::new_in_workspace(self.skills_dir, workspace_root.clone()),
            [
                trigger::builtin_trigger_skill(),
                remi_skill::builtin_remi_skill(),
            ],
        ));
        let data_dir = memory.data_dir.clone();
        let sandbox = self.sandbox_config.build()?;
        let agents_dir = self.agents_dir.clone();
        install_embedded_agent_profiles(&agents_dir)?;
        let active_agent_id = self.active_agent_id.clone();
        let todo_backend = Arc::new(todo::HybridTodoBackend::new(data_dir.clone()));
        let trigger_backend = Arc::new(trigger::TriggerBackend::new(data_dir.clone()));
        let approval_manager = ToolApprovalManager::new();
        let user_question_manager = UserQuestionManager::new();
        let hook_manager = HookManager::new(workspace_root.clone(), data_dir.clone());
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
            trigger_backend: Arc::clone(&trigger_backend),
            acp_backend: Arc::clone(&acp_backend),
            sandbox: Arc::clone(&sandbox),
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
            user_question_manager: Arc::clone(&user_question_manager),
            hook_manager: Arc::clone(&hook_manager),
            overflow_bytes,
        };
        let mut acp_local_tools = DefaultToolRegistry::new();
        skill::register_skill_tools(&mut acp_local_tools, Arc::clone(&skill_store));
        todo::register_todo_tools(&mut acp_local_tools, Arc::clone(&todo_backend));
        trigger::register_trigger_tools(&mut acp_local_tools, Arc::clone(&trigger_backend));
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
        acp_local_tools.register(MemoryGetDetailTool {
            store: Arc::clone(&memory),
            agent_id: active_agent_id.clone(),
        });
        acp_local_tools.register(MemoryUpsertNamedTool {
            store: Arc::clone(&memory),
            agent_id: active_agent_id.clone(),
            workspace_root: self.sandbox_config.host_dir().to_path_buf(),
        });
        acp_local_tools.register(MemoryRecallTool {
            store: Arc::clone(&memory),
            agent_id: active_agent_id.clone(),
        });
        acp_local_tools.register(SearchTool {
            skill_store: Arc::clone(&skill_store),
            memory_store: Arc::clone(&memory),
            agent_id: active_agent_id.clone(),
        });
        if self.sandbox_config.bash_enabled() {
            acp_local_tools.register(WorkspaceBashTool::new(
                Arc::clone(&sandbox),
                Arc::clone(&redactor),
            ));
        }
        acp_local_tools.register(WorkspaceSshTool::new(Arc::clone(&redactor)));
        acp_local_tools.register(RootedFsReadTool {
            sandbox: Arc::clone(&sandbox),
            redactor: Arc::clone(&redactor),
        });
        acp_local_tools.register(RootedFsWriteTool {
            sandbox: Arc::clone(&sandbox),
        });
        acp_local_tools.register(RootedFsApplyPatchTool {
            sandbox: Arc::clone(&sandbox),
        });
        acp_local_tools.register(RootedFsCreateTool {
            sandbox: Arc::clone(&sandbox),
        });
        acp_local_tools.register(RootedFsRemoveTool {
            sandbox: Arc::clone(&sandbox),
        });
        acp_local_tools.register(RootedFsLsTool {
            sandbox: Arc::clone(&sandbox),
            redactor: Arc::clone(&redactor),
        });
        register_fetch_tool(
            &mut acp_local_tools,
            workspace_root.clone(),
            sandbox.workspace_root_label(),
            sandbox.kind() != "docker",
            self.im_bridge.clone(),
        );
        acp_local_tools.register(ExaSearchTool::new());
        acp_local_tools.register(NowTool);
        acp_local_tools.register(SleepTool);
        acp_local_tools.register(ManageYourselfTool);
        acp_local_tools.register(AskUserQuestionTool::new(Arc::clone(&user_question_manager)));
        let run_locks: ThreadRunLocks = Arc::new(AsyncMutex::new(HashMap::new()));
        acp_backend.set_local_runner(Arc::new(LocalAcpAgentRunner {
            agent: CatAgent {
                inner: acp_local_inner,
                local_tools: acp_local_tools,
                data_dir: memory.data_dir.clone(),
                workspace_root: workspace_root.clone(),
                workspace_root_label: sandbox.workspace_root_label(),
                allow_host_absolute_paths: sandbox.kind() != "docker",
                overflow_bytes,
                im_bridge: self.im_bridge.clone(),
                tool_allowlist: self.tool_allowlist.clone(),
                approval_manager: Arc::clone(&approval_manager),
                user_question_manager: Arc::clone(&user_question_manager),
                hook_manager: Arc::clone(&hook_manager),
            },
            memory: Arc::clone(&memory),
            run_locks: Arc::clone(&run_locks),
        }));
        let tool_deps = LocalToolDeps {
            skill_store: Arc::clone(&skill_store),
            memory: Arc::clone(&memory),
            todo_backend: Arc::clone(&todo_backend),
            trigger_backend: Arc::clone(&trigger_backend),
            acp_backend: Arc::clone(&acp_backend),
            sandbox: Arc::clone(&sandbox),
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
            user_question_manager: Arc::clone(&user_question_manager),
            hook_manager: Arc::clone(&hook_manager),
            overflow_bytes,
        };
        let local_tools = tool_deps.build_tools(&profile, self.extra_options.clone(), true);
        let mut model_agents = HashMap::new();
        for model_profile in self.model_registry.list() {
            if model_profile.id == profile.id {
                continue;
            }
            let model_extra_options = model_profile.merged_extra_options(None)?;
            let model_inner = build_inner_agent(
                &self.api_key,
                model_profile,
                system_prompt.clone(),
                self.max_turns,
                model_extra_options.clone(),
            );
            let model_tools = tool_deps.build_tools(model_profile, model_extra_options, true);
            let model_overflow_bytes = self.overflow_bytes.unwrap_or(model_profile.overflow_bytes);
            model_agents.insert(
                model_profile.id.clone(),
                CatAgent {
                    inner: model_inner,
                    local_tools: model_tools,
                    data_dir: memory.data_dir.clone(),
                    workspace_root: workspace_root.clone(),
                    workspace_root_label: sandbox.workspace_root_label(),
                    allow_host_absolute_paths: sandbox.kind() != "docker",
                    overflow_bytes: model_overflow_bytes,
                    im_bridge: self.im_bridge.clone(),
                    tool_allowlist: self.tool_allowlist.clone(),
                    approval_manager: Arc::clone(&approval_manager),
                    user_question_manager: Arc::clone(&user_question_manager),
                    hook_manager: Arc::clone(&hook_manager),
                },
            );
        }

        Ok(CatBot {
            inner: CatAgent {
                inner: inner_loop,
                local_tools,
                data_dir: memory.data_dir.clone(),
                workspace_root: workspace_root.clone(),
                workspace_root_label: sandbox.workspace_root_label(),
                allow_host_absolute_paths: sandbox.kind() != "docker",
                overflow_bytes,
                im_bridge: self.im_bridge,
                tool_allowlist: self.tool_allowlist,
                approval_manager: Arc::clone(&approval_manager),
                user_question_manager: Arc::clone(&user_question_manager),
                hook_manager: Arc::clone(&hook_manager),
            },
            model_agents,
            skill_store,
            memory,
            todo_backend,
            trigger_backend,
            acp_backend,
            approval_manager,
            user_question_manager,
            hook_manager,
            run_locks,
            model_profile: profile,
            model_registry: Arc::clone(&self.model_registry),
            api_key: self.api_key,
            redactor,
        })
    }
}

fn delegate_tool_name(agent_id: &str) -> String {
    format!("agent__{}", agent_id.trim().replace('-', "_"))
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

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
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
            local_tools: build_subagent_tools(&self.deps, &self.profile),
            data_dir: self.data_dir.clone(),
            workspace_root: self.deps.workspace_root.clone(),
            workspace_root_label: self.deps.sandbox.workspace_root_label(),
            allow_host_absolute_paths: self.deps.sandbox.kind() != "docker",
            overflow_bytes: self.overflow_bytes,
            im_bridge: None,
            tool_allowlist: Some(self.tool_allowlist.clone()),
            approval_manager: Arc::clone(&self.deps.approval_manager),
            user_question_manager: Arc::clone(&self.deps.user_question_manager),
            hook_manager: Arc::clone(&self.deps.hook_manager),
        };
        let sub_thread_id = ThreadId(match named.as_deref() {
            Some(named) => format!("subagent:{}:{named}", self.agent_name),
            None => format!("subagent:{}", uuid::Uuid::new_v4()),
        });
        let sub_run_id = RunId(uuid::Uuid::new_v4().to_string());
        let sub_thread_id_for_memory = sub_thread_id.0.clone();
        let metadata = subagent_metadata(ctx, &sub_thread_id_for_memory);
        let memory = Arc::clone(&self.deps.memory);
        let persistent = named.is_some();
        let session_lock = if persistent {
            Some(self.session_lock(&sub_thread_id_for_memory).await)
        } else {
            None
        };

        Ok(ToolResult::Output(stream! {
            let _session_guard = match session_lock.as_ref() {
                Some(lock) => Some(lock.lock().await),
                None => None,
            };
            let mut sub_task = task.clone();
            let sub_hook_context = HookContext {
                session_id: sub_thread_id_for_memory.clone(),
                transcript_path: None,
                cwd: self.deps.workspace_root.clone(),
                model: Some(self.model_name.clone()),
                turn_id: Some(sub_run_id.0.clone()),
                permission_mode: None,
            };
            let start_hook = self
                .deps
                .hook_manager
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
                SubSessionEventPayload::Start,
            ));

            let mut final_output = String::new();
            let mut skip_count = 0usize;
            let mut input = LoopInput::start(&sub_task).metadata(metadata.clone());
            if persistent {
                match memory.load_context(&sub_thread_id_for_memory).await {
                    Ok(mut ctx) => {
                        let history = build_injected_history(&ctx);
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
                            SubSessionEventPayload::Error {
                                message: format!("failed to load sub-agent session: {err}"),
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
                            SubSessionEventPayload::Delta { content },
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
                            SubSessionEventPayload::ThinkingEnd { content },
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
                            SubSessionEventPayload::ToolCallStart { id, name },
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
                            SubSessionEventPayload::ToolCallArgumentsDelta { id, delta },
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
                            SubSessionEventPayload::ToolResult { id, name, result },
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
                        let stop_hook = self
                            .deps
                            .hook_manager
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
                                    let history = build_injected_history(&ctx);
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
                            SubSessionEventPayload::Done {
                                final_output: if final_output.trim().is_empty() {
                                    None
                                } else {
                                    Some(final_output.clone())
                                },
                            },
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
                            SubSessionEventPayload::Error {
                                message: message.clone(),
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
                SubSessionEventPayload::Done {
                    final_output: if final_output.trim().is_empty() {
                        None
                    } else {
                        Some(final_output.clone())
                    },
                },
            ));
            yield ToolOutput::text(format_subagent_tool_result(
                &sub_thread_id,
                &sub_run_id,
                &agent_name,
                true,
                &final_output,
                None,
            ));
        }))
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

fn subagent_metadata(ctx: &ToolContext, sub_thread_id: &str) -> serde_json::Value {
    let mut metadata = ctx
        .metadata
        .clone()
        .unwrap_or_else(|| serde_json::json!({}));
    if !metadata.is_object() {
        metadata = serde_json::json!({});
    }
    if let serde_json::Value::Object(map) = &mut metadata {
        map.insert(
            "thread_id".to_string(),
            serde_json::Value::String(sub_thread_id.to_string()),
        );
        if !map.contains_key("parent_thread_id") {
            if let Some(thread_id) = &ctx.thread_id {
                map.insert(
                    "parent_thread_id".to_string(),
                    serde_json::Value::String(thread_id.0.clone()),
                );
            }
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
        let system_prompt = profile.system_prompt.clone();
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

fn build_subagent_tools(deps: &LocalToolDeps, profile: &AgentProfile) -> DefaultToolRegistry {
    let mut local_tools = DefaultToolRegistry::new();
    skill::register_skill_tools(&mut local_tools, Arc::clone(&deps.skill_store));
    todo::register_todo_tools(&mut local_tools, Arc::clone(&deps.todo_backend));
    trigger::register_trigger_tools(&mut local_tools, Arc::clone(&deps.trigger_backend));
    local_tools.register(MemoryGetDetailTool {
        store: Arc::clone(&deps.memory),
        agent_id: profile.id.clone(),
    });
    local_tools.register(MemoryUpsertNamedTool {
        store: Arc::clone(&deps.memory),
        agent_id: profile.id.clone(),
        workspace_root: deps.workspace_root.clone(),
    });
    local_tools.register(MemoryRecallTool {
        store: Arc::clone(&deps.memory),
        agent_id: profile.id.clone(),
    });
    local_tools.register(SearchTool {
        skill_store: Arc::clone(&deps.skill_store),
        memory_store: Arc::clone(&deps.memory),
        agent_id: profile.id.clone(),
    });
    if deps.bash_enabled {
        local_tools.register(WorkspaceBashTool::new(
            Arc::clone(&deps.sandbox),
            Arc::clone(&deps.redactor),
        ));
    }
    local_tools.register(WorkspaceSshTool::new(Arc::clone(&deps.redactor)));
    local_tools.register(RootedFsReadTool {
        sandbox: Arc::clone(&deps.sandbox),
        redactor: Arc::clone(&deps.redactor),
    });
    local_tools.register(RootedFsWriteTool {
        sandbox: Arc::clone(&deps.sandbox),
    });
    local_tools.register(RootedFsApplyPatchTool {
        sandbox: Arc::clone(&deps.sandbox),
    });
    local_tools.register(RootedFsCreateTool {
        sandbox: Arc::clone(&deps.sandbox),
    });
    local_tools.register(RootedFsRemoveTool {
        sandbox: Arc::clone(&deps.sandbox),
    });
    local_tools.register(RootedFsLsTool {
        sandbox: Arc::clone(&deps.sandbox),
        redactor: Arc::clone(&deps.redactor),
    });
    register_fetch_tool(
        &mut local_tools,
        deps.workspace_root.clone(),
        deps.sandbox.workspace_root_label(),
        deps.sandbox.kind() != "docker",
        deps.im_bridge.clone(),
    );
    local_tools.register(ExaSearchTool::new());
    local_tools.register(NowTool);
    local_tools.register(SleepTool);
    local_tools.register(ManageYourselfTool);
    local_tools
}

#[cfg(test)]
mod tests {
    use super::{
        append_thread_todo_system_prompt, context_percent_tokens, default_system_prompt,
        format_subagent_tool_result, insert_single_chat_sender_system_prompt,
        install_embedded_model_profiles, local_acp_thread_id, model_input_snapshot_from_loop_input,
        prepend_group_sender_username, route_thread_todo_prompt, single_chat_sender_system_prompt,
        thread_run_lock, AgentModelBindings, CatBotBuilder, CatEvent, Content, ContentPart,
        GoalMaxRounds, LlmCompressor, LoopInput, Message, ModelProfileRegistry,
        PartialTurnRecorder, RemiSubAgentTool, SandboxConfig, StreamOptions, ThreadRunLocks,
        DEFAULT_AGENT_ID, DEFAULT_AUTO_COMPRESS_CONTEXT_PERCENT,
    };
    use crate::{estimate_model_input_tokens, ModelInputSegmentCategory};
    use crate::memory::{build_injected_history, MemoryContext, MemoryIndex};
    use crate::model_profile::ModelProfileConfig;
    use crate::todo::tools::TodoItem;
    use futures::StreamExt as _;
    use remi_agentloop::prelude::Role;
    use remi_agentloop::tool::registry::ToolRegistry;
    use remi_agentloop::types::{FunctionCall, RunId, ThreadId, ToolCallMessage};
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;
    use uuid::Uuid;

    fn test_model_profile() -> ModelProfileConfig {
        ModelProfileConfig {
            id: "default".to_string(),
            name: "Default".to_string(),
            description: None,
            provider: None,
            model: "gpt-4o".to_string(),
            base_url: Some("https://api.openai.com/v1".to_string()),
            thinking: None,
            max_output_tokens: 4096,
            context_tokens: 128000,
            supports_images: true,
            short_term_tokens: 8192,
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
            short_term_tokens: None,
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
            agents_dir: agents_dir.clone(),
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
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
    fn persistent_delegate_agent_exposes_named_parameter() {
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
            short_term_tokens: None,
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
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
        }
        .agent_profile(root_profile)
        .unwrap()
        .build()
        .unwrap();

        let definitions = bot.inner.local_tools.definitions(&serde_json::Value::Null);
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
            short_term_tokens: None,
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
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
        }
        .build()
        .unwrap();

        let cancel = Arc::new(tokio::sync::Notify::new());
        let mut stream = std::pin::pin!(bot.stream_with_options(
            "cancel-partial-thread",
            Content::text("start"),
            StreamOptions {
                cancel: Some(Arc::clone(&cancel)),
                ..StreamOptions::default()
            },
        ));
        let mut saw_partial = false;
        while let Some(event) = stream.next().await {
            match event {
                CatEvent::Text(text) => {
                    if text.contains("partial answer") {
                        saw_partial = true;
                        cancel.notify_one();
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
            short_term_tokens: None,
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
            agents_dir: PathBuf::from("agents"),
            max_turns: None,
            model_registry: Arc::new(ModelProfileRegistry::load(PathBuf::from("models")).unwrap()),
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
                max_output_tokens: 393216,
                context_tokens: 1_000_000,
                supports_images: false,
                short_term_tokens: 16000,
                overflow_bytes: 24000,
                auto_compress: true,
                extra_options: serde_json::Map::new(),
            },
            runtime_model_locked: true,
            system: default_system_prompt(),
            skills_dir: PathBuf::from("skills"),
            data_dir: PathBuf::from("data"),
            agent_md_path: None,
            short_term_tokens: None,
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
            agents_dir: PathBuf::from("agents"),
            max_turns: None,
            model_registry: Arc::new(ModelProfileRegistry::load(PathBuf::from("models")).unwrap()),
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
            short_term_tokens: None,
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
            agents_dir,
            max_turns: Some(2),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
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
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
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
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
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

    #[tokio::test]
    async fn active_supervisor_routes_existing_todo_only_to_supervisor_end_to_end() {
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
            short_term_tokens: None,
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
            agents_dir,
            max_turns: Some(8),
            model_registry: Arc::new(ModelProfileRegistry::load(models_dir).unwrap()),
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
                storage_kind: Default::default(),
                collection_uuid: None,
                thing_uuid: None,
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
