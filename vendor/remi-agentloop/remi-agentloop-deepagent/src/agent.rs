//! `DeepAgent` — the top-level agent that wires together all layers.
//!
//! ## Architecture
//!
//! ```text
//! DeepAgent
//!   ├── inner: BuiltAgent<M, InMemoryStore, NoCheckpointStore>
//! │        └── AgentLoop<M>  (bash + fs tools, registered locally)
//! │   ├── todo_tools: DefaultToolRegistry   ("todo__*")
//! │   ├── skill_tools: DefaultToolRegistry  ("skill__*")  ← backed by FileSkillStore
//!  │   └── task_tool: SubAgentTaskTool       ("task__run")
//! ```
//!
//! `DeepAgent::chat()` runs a resumption loop:
//! 1. Inject all tool definitions into `LoopInput::Start`.
//! 2. Drive the inner stream; pass most events through.
//! 3. On `NeedToolExecution`: execute todo/skill/task tools locally,
//!    emit the corresponding `DeepAgentEvent` side-events, then resume
//!    the inner agent with `LoopInput::Resume`.
//! 4. Repeat until `Done` or `Error`.

use crate::workspace_fs::{
    RootedFsCreateTool, RootedFsLsTool, RootedFsReadTool, RootedFsRemoveTool, RootedFsWriteTool,
    WorkspaceBashTool,
};
use async_stream::stream;
use futures::{Stream, StreamExt};
use remi_core::agent::Agent;
use remi_core::builder::{AgentBuilder, BuiltAgent};
use remi_core::checkpoint::NoCheckpointStore;
use remi_core::config::AgentConfig;
use remi_core::context::NoStore;
use remi_core::error::AgentError;
use remi_core::model::ChatModel;
use remi_core::state::AgentState;
use remi_core::tool::registry::{DefaultToolRegistry, ToolRegistry};
use remi_core::tool::{Tool, ToolContext, ToolDefinition, ToolDefinitionContext, ToolOutput};
use remi_core::types::{AgentEvent, LoopInput, Message, ParsedToolCall, ToolCallOutcome};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::events::{DeepAgentEvent, SkillEvent, TodoEvent};
use crate::registry::FileBackedRegistry;
use crate::search::ExaSearchTool;
use crate::skill::store::FileSkillStore;
use crate::skill::tools::{SkillDeleteTool, SkillGetTool, SkillListTool, SkillSaveTool};
use crate::task::SubAgentTaskTool;
use crate::todo::tools::{
    TodoAddTool, TodoCompleteTool, TodoListTool, TodoRemoveTool, TodoUpdateTool,
};

// ── DeepAgent ─────────────────────────────────────────────────────────────────

/// Fully assembled deep agent.
pub struct DeepAgent<M: ChatModel + Clone + Send + Sync + 'static> {
    inner: BuiltAgent<M, NoStore, NoCheckpointStore>,
    /// All "virtual" tools handled by the layer loop (todo, skill, task).
    local_tools: DefaultToolRegistry,
}

impl<M: ChatModel + Clone + Send + Sync + 'static> DeepAgent<M> {
    fn local_tool_definition_context(input: &LoopInput) -> ToolDefinitionContext {
        match input {
            LoopInput::Start {
                metadata,
                user_state,
                ..
            } => ToolDefinitionContext {
                metadata: metadata.clone(),
                user_state: user_state.clone().unwrap_or(serde_json::Value::Null),
                ..ToolDefinitionContext::default()
            },
            LoopInput::Resume { state, .. } | LoopInput::Cancel { state } => {
                ToolDefinitionContext {
                    thread_id: Some(state.thread_id.clone()),
                    run_id: Some(state.run_id.clone()),
                    metadata: state.config.metadata.clone(),
                    user_state: state.user_state.clone(),
                }
            }
        }
    }

    /// Send a message and receive a stream of `DeepAgentEvent`.
    pub async fn chat(
        &self,
        message: impl Into<String>,
    ) -> Result<impl Stream<Item = DeepAgentEvent> + '_, AgentError> {
        let input = LoopInput::start(message.into());
        self.run(input).await
    }

    /// Like [`chat`] but carries forward the conversation history from a
    /// previous run. Pass the `Vec<Message>` you received via
    /// [`DeepAgentEvent::History`] to maintain multi-turn context.
    pub async fn chat_with_history(
        &self,
        message: impl Into<String>,
        history: Vec<Message>,
    ) -> Result<impl Stream<Item = DeepAgentEvent> + '_, AgentError> {
        let input = LoopInput::start(message.into()).history(history);
        self.run(input).await
    }

    /// Low-level entry point — accepts any [`LoopInput`].
    pub async fn run(
        &self,
        input: LoopInput,
    ) -> Result<impl Stream<Item = DeepAgentEvent> + '_, AgentError> {
        Ok(self.drive(input))
    }

    // ── Internal drive loop ───────────────────────────────────────────────────

    fn drive<'a>(&'a self, input: LoopInput) -> impl Stream<Item = DeepAgentEvent> + 'a {
        // Cache local tool definitions for this input.
        let extra_defs = self
            .local_tools
            .definitions_with_context(&Self::local_tool_definition_context(&input));

        stream! {
            let mut current = inject_extra_tools(input, extra_defs);
            let mut last_messages: Vec<Message> = vec![];

            loop {
                // ── Call inner agent ──────────────────────────────────────────
                let inner_stream = match self.inner.chat(current).await {
                    Ok(s) => s,
                    Err(e) => {
                        yield DeepAgentEvent::Agent(AgentEvent::Error(e));
                        return;
                    }
                };
                let mut inner_stream = std::pin::pin!(inner_stream);

                let mut next_input: Option<LoopInput> = None;

                // ── Drive inner stream ────────────────────────────────────────
                while let Some(ev) = inner_stream.next().await {
                    match ev {
                        // ── Our tools need execution ──────────────────────────
                        AgentEvent::NeedToolExecution {
                            mut state,
                            tool_calls,
                            completed_results,
                        } => {
                            let (local, external): (Vec<_>, Vec<_>) = tool_calls
                                .iter()
                                .cloned()
                                .partition(|tc| self.local_tools.contains(&tc.name));

                            let tool_ctx = build_tool_ctx(&state);
                            let resume_map = HashMap::new();
                            let mut all_outcomes: Vec<ToolCallOutcome> = completed_results;

                            if !local.is_empty() {
                                let results = self.local_tools
                                    .execute_parallel(&local, &resume_map, &tool_ctx)
                                    .await;

                                for (call_id, result) in results {
                                    let tc = local.iter().find(|t| t.id == call_id).unwrap();

                                    let result_str = match result {
                                        Err(e) => format!("error: {e}"),
                                        Ok(remi_core::tool::ToolResult::Interrupt(_)) => {
                                            "interrupted".to_string()
                                        }
                                        Ok(remi_core::tool::ToolResult::Output(mut s)) => {
                                            let mut last = String::new();
                                            while let Some(out) = s.next().await {
                                                if let ToolOutput::Result(c) = out {
                                                    last = c.text_content();
                                                }
                                            }
                                            last
                                        }
                                    };

                                    // Emit specialised layer events
                                    for deep_ev in make_deep_events(tc, &result_str, &tool_ctx) {
                                        yield deep_ev;
                                    }

                                    all_outcomes.push(ToolCallOutcome::Result {
                                        tool_call_id: call_id,
                                        tool_name: tc.name.clone(),
                                        content: remi_core::types::Content::text(result_str),
                                    });
                                }

                                // Write back user_state mutations from todo tools
                                state.user_state =
                                    tool_ctx.user_state.read().unwrap().clone();
                            }

                            if !external.is_empty() {
                                // Propagate unhandled externals upward
                                yield DeepAgentEvent::Agent(AgentEvent::NeedToolExecution {
                                    state,
                                    tool_calls: external,
                                    completed_results: all_outcomes,
                                });
                                return;
                            }

                            // All handled — schedule resume
                            next_input = Some(LoopInput::Resume {
                                state,
                                results: all_outcomes,
                            });
                            break; // break inner while, outer loop will re-call inner
                        }

                        // ── Terminal events ────────────────────────────────────
                        AgentEvent::Done => {
                            yield DeepAgentEvent::History(last_messages.clone());
                            yield DeepAgentEvent::Agent(AgentEvent::Done);
                            return;
                        }
                        AgentEvent::Cancelled
                        | AgentEvent::Error(_) => {
                            yield DeepAgentEvent::Agent(ev);
                            return;
                        }

                        // ── Capture full message list from checkpoints ─────────
                        AgentEvent::Checkpoint(ref cp) => {
                            last_messages = cp.state.messages.clone();
                            // don't propagate checkpoint events to consumers
                        }

                        // ── Pass-through ───────────────────────────────────────
                        other => yield DeepAgentEvent::Agent(other),
                    }
                }

                // inner_stream goes out of scope here; borrow on self.inner ends.

                match next_input {
                    Some(n) => current = n,
                    None => return,
                }
            }
        }
    }

    /// Flush any buffered tracing I/O (e.g. pending LangSmith HTTP calls).
    ///
    /// Call this once after the stream returned by `chat()` / `chat_with_history()`
    /// has been fully consumed — before the async runtime shuts down.
    pub async fn flush_tracer(&self) {
        self.inner.flush_tracer().await;
    }
}

// ── DeepAgentBuilder ──────────────────────────────────────────────────────────

/// Builder for [`DeepAgent`].
///
/// # Example
/// ```no_run
/// use remi_deepagent::DeepAgentBuilder;
///
/// # async fn example() {
/// let agent = DeepAgentBuilder::new(model)
///     .system("You are a helpful coding assistant.")
///     .max_turns(20)
///     .skills_dir(".deepagent/skills")
///     .result_spill_threshold(4096)
///     .build();
/// # }
/// ```
pub struct DeepAgentBuilder<M: ChatModel + Clone + Send + Sync + 'static> {
    model: M,
    system: String,
    max_turns: usize,
    /// Workspace root: bash cwd, fs tool root, and parent of skills dir.
    workspace_dir: PathBuf,
    skills_dir: Option<PathBuf>, // None = <workspace_dir>/.claude/skills
    result_spill_threshold: usize,
    task_sub_agent_turns: usize,
    search_api_key: Option<String>,
    langsmith_api_key: Option<String>,
    langsmith_project: Option<String>,
    /// Model name string for tracing (e.g. "kimi-k2.5"). Does not change which
    /// model is called — that is determined by the `M: ChatModel` instance.
    model_name: Option<String>,
    extra_tools: Vec<Box<dyn FnOnce(&mut DefaultToolRegistry)>>,
}

impl<M: ChatModel + Clone + Send + Sync + 'static> DeepAgentBuilder<M> {
    pub fn new(model: M) -> Self {
        Self {
            model,
            system: "You are a highly capable AI assistant. \
                You have access to bash, filesystem, todo list, skill memory, \
                and sub-agent task delegation tools. \
                Use todo__add/complete to track multi-step work. \
                Use skill__save to record reusable procedures for future sessions. \
                Use task__run to delegate focused subtasks to a worker agent. \
                All file paths are relative to the workspace directory."
                .to_string(),
            max_turns: 20,
            workspace_dir: PathBuf::from(".deepagent/workspace"),
            skills_dir: None,
            result_spill_threshold: 4096,
            task_sub_agent_turns: 10,
            search_api_key: None,
            langsmith_api_key: None,
            langsmith_project: None,
            model_name: None,
            extra_tools: vec![],
        }
    }

    pub fn system(mut self, s: impl Into<String>) -> Self {
        self.system = s.into();
        self
    }

    pub fn max_turns(mut self, n: usize) -> Self {
        self.max_turns = n;
        self
    }

    /// Set the workspace root directory (default: `.deepagent/workspace`).
    /// Bash runs with this as its working directory.
    /// All fs tool paths are resolved relative to this directory.
    pub fn workspace_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace_dir = path.into();
        self
    }

    /// Override the skills directory (default: `<workspace_dir>/.claude/skills`).
    /// Skills follow the Claude Code convention: one sub-directory per skill
    /// containing a `SKILL.md` file (with optional YAML frontmatter).
    pub fn skills_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.skills_dir = Some(path.into());
        self
    }

    /// Enable Exa web search with the given API key.
    pub fn exa_api_key(mut self, key: impl Into<String>) -> Self {
        self.search_api_key = Some(key.into());
        self
    }

    /// Enable Exa web search if `EXA_API_KEY` env var is set.
    pub fn exa_from_env(mut self) -> Self {
        if let Ok(k) = std::env::var("EXA_API_KEY") {
            self.search_api_key = Some(k);
        }
        self
    }

    /// Enable LangSmith tracing with the given API key.
    pub fn langsmith_api_key(mut self, key: impl Into<String>) -> Self {
        self.langsmith_api_key = Some(key.into());
        self
    }

    /// Set the LangSmith project name (default: `"deep-agent"`).
    pub fn langsmith_project(mut self, project: impl Into<String>) -> Self {
        self.langsmith_project = Some(project.into());
        self
    }

    /// Enable LangSmith tracing if the `LANGSMITH_API_KEY` env var is set.
    pub fn langsmith_from_env(mut self) -> Self {
        if let Ok(k) = std::env::var("LANGSMITH_API_KEY") {
            self.langsmith_api_key = Some(k);
        }
        self
    }

    /// Set the model name used for tracing/observability.
    /// (Does not change which model is called — use the `model` constructor argument for that.)
    pub fn model_name(mut self, name: impl Into<String>) -> Self {
        self.model_name = Some(name.into());
        self
    }

    /// Tool output byte threshold for spilling to file (default 4 KiB).
    pub fn result_spill_threshold(mut self, bytes: usize) -> Self {
        self.result_spill_threshold = bytes;
        self
    }

    pub fn task_sub_agent_turns(mut self, n: usize) -> Self {
        self.task_sub_agent_turns = n;
        self
    }

    /// Register an additional tool into the inner `AgentLoop`.
    pub fn tool(mut self, tool: impl Tool + Send + Sync + 'static) -> Self {
        self.extra_tools
            .push(Box::new(move |r: &mut DefaultToolRegistry| {
                r.register(tool);
            }));
        self
    }

    pub fn build(self) -> DeepAgent<M> {
        // ── Ensure workspace dir exists ───────────────────────────────────────
        let workspace_dir = self.workspace_dir.clone();
        let _ = std::fs::create_dir_all(&workspace_dir);

        // Skills follow the Claude Code convention:
        // <workspace>/.claude/skills/<name>/SKILL.md
        let skills_dir = self
            .skills_dir
            .clone()
            .unwrap_or_else(|| workspace_dir.join(".claude").join("skills"));
        let _ = std::fs::create_dir_all(&skills_dir);

        // ── Auto-migrate legacy flat skills (workspace/skills/<name>.md) ───────
        // If the old flat-file skills dir exists, convert each file to the new
        // subdirectory format (<name>/SKILL.md) and remove the originals.
        let legacy_skills_dir = workspace_dir.join("skills");
        if legacy_skills_dir.is_dir() && legacy_skills_dir != skills_dir {
            if let Ok(rd) = std::fs::read_dir(&legacy_skills_dir) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md") {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            let dest_dir = skills_dir.join(stem);
                            let dest = dest_dir.join("SKILL.md");
                            if !dest.exists() {
                                let _ = std::fs::create_dir_all(&dest_dir);
                                if let Ok(content) = std::fs::read_to_string(&p) {
                                    if std::fs::write(&dest, content).is_ok() {
                                        let _ = std::fs::remove_file(&p);
                                    }
                                }
                            }
                        }
                    }
                }
                // Remove the now-empty legacy directory (ignore errors)
                let _ = std::fs::remove_dir(&legacy_skills_dir);
            }
        }

        // Spill dir inside workspace
        let spill_dir = workspace_dir.join(".tool-results");

        // ── Load SOUL.md (identity / values) and prepend to system prompt ─────
        let soul_path = workspace_dir.join("SOUL.md");
        // Auto-create a default SOUL.md on first run so the user has a template
        // to customise rather than starting from nothing.
        if !soul_path.exists() {
            let default_soul = "\
# Identity & Values\n\
\n\
<!-- Edit this file to give the agent a persistent personality, style, and\n\
     values that apply to every conversation. This content is prepended to\n\
     the system prompt automatically. -->\n\
\n\
You are a helpful, thoughtful AI assistant. Be concise, honest, and precise.\n\
When unsure, say so. Prefer working solutions over long explanations.\n";
            let _ = std::fs::write(&soul_path, default_soul);
        }
        let soul_content = std::fs::read_to_string(&soul_path).unwrap_or_default();
        let mut system = if soul_content.trim().is_empty() {
            self.system.clone()
        } else {
            format!(
                "{soul}\n\n{base}",
                soul = soul_content.trim_end(),
                base = self.system
            )
        };

        // ── Inject available skills summary into system prompt ─────────────────
        // Load skill names + descriptions now so the model knows which skills
        // exist without needing to call skill__list first.
        let skill_store_preview = FileSkillStore::new(&skills_dir);
        let skill_list = skill_store_preview.list_with_descriptions_sync();
        if !skill_list.is_empty() {
            system.push_str(
                "\n\n## Saved Skills\n\
                 The following skills are saved in `.claude/skills/`. \
                 Use `skill__get <name>` to load the full content.\n",
            );
            for (name, desc) in &skill_list {
                match desc {
                    Some(d) => system.push_str(&format!("- **{name}**: {d}\n")),
                    None => system.push_str(&format!("- **{name}**\n")),
                }
            }
        }

        // ── Inner agent (bash + fs, FileBackedRegistry) ───────────────────────
        let mut inner_registry = DefaultToolRegistry::new();
        inner_registry.register(WorkspaceBashTool::new(workspace_dir.clone()));
        inner_registry.register(RootedFsReadTool {
            root: workspace_dir.clone(),
        });
        inner_registry.register(RootedFsWriteTool {
            root: workspace_dir.clone(),
        });
        inner_registry.register(RootedFsCreateTool {
            root: workspace_dir.clone(),
        });
        inner_registry.register(RootedFsRemoveTool {
            root: workspace_dir.clone(),
        });
        inner_registry.register(RootedFsLsTool {
            root: workspace_dir.clone(),
        });
        if let Some(key) = self.search_api_key.clone() {
            inner_registry.register(ExaSearchTool::new(key));
        }
        for apply in self.extra_tools {
            apply(&mut inner_registry);
        }
        let file_backed = FileBackedRegistry::new(inner_registry)
            .threshold(self.result_spill_threshold)
            .output_dir(spill_dir)
            .workspace_root(workspace_dir.clone());
        let mut core_b = AgentBuilder::new()
            .model(self.model.clone())
            .system(&system)
            .max_turns(self.max_turns);

        if let Some(name) = self.model_name.clone() {
            core_b = core_b.config(AgentConfig::default().with_model(name));
        }

        #[cfg(feature = "tracing-langsmith")]
        if let Some(key) = self.langsmith_api_key.clone() {
            let project = self
                .langsmith_project
                .clone()
                .unwrap_or_else(|| "deep-agent".to_string());
            let tracer = remi_core::tracing::LangSmithTracer::new(key).with_project(project);
            core_b = core_b.tracer(tracer);
        }

        let inner = core_b.with_registry(file_backed).build();

        // ── Local "virtual" tools (todo + skill + task) ───────────────────────
        let skill_store = Arc::new(FileSkillStore::new(skills_dir));
        let mut local_tools = DefaultToolRegistry::new();

        // Todo
        local_tools.register(TodoAddTool);
        local_tools.register(TodoListTool);
        local_tools.register(TodoCompleteTool);
        local_tools.register(TodoUpdateTool);
        local_tools.register(TodoRemoveTool);

        // Skill
        local_tools.register(SkillSaveTool {
            store: Arc::clone(&skill_store),
        });
        local_tools.register(SkillGetTool {
            store: Arc::clone(&skill_store),
        });
        local_tools.register(SkillListTool {
            store: Arc::clone(&skill_store),
        });
        local_tools.register(SkillDeleteTool {
            store: Arc::clone(&skill_store),
        });

        // Task (sub-agent delegation)
        local_tools.register(SubAgentTaskTool::new(
            self.model.clone(),
            "You are a focused worker agent. Complete the given task and return a \
             clear, complete response. You have bash and filesystem tools.",
            self.task_sub_agent_turns,
        ));

        DeepAgent::<M> { inner, local_tools }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn inject_extra_tools(input: LoopInput, extra: Vec<ToolDefinition>) -> LoopInput {
    match input {
        LoopInput::Start {
            content,
            history,
            mut extra_tools,
            model,
            temperature,
            max_tokens,
            metadata,
            message_metadata,
            user_name,
            user_state,
        } => {
            extra_tools.extend(extra);
            LoopInput::Start {
                content,
                history,
                extra_tools,
                model,
                temperature,
                max_tokens,
                metadata,
                message_metadata,
                user_name,
                user_state,
            }
        }
        other => other,
    }
}

fn build_tool_ctx(state: &AgentState) -> ToolContext {
    let user_state = Arc::new(std::sync::RwLock::new(state.user_state.clone()));
    ToolContext {
        config: AgentConfig::default(),
        thread_id: Some(state.thread_id.clone()),
        run_id: state.run_id.clone(),
        metadata: state.config.metadata.clone(),
        cancel: None,
        user_state,
    }
}

/// Derive `DeepAgentEvent` side-events for a completed tool call.
fn make_deep_events(tc: &ParsedToolCall, result: &str, ctx: &ToolContext) -> Vec<DeepAgentEvent> {
    let mut evs = vec![];
    match tc.name.as_str() {
        "todo__add" => {
            let content = tc.arguments["content"].as_str().unwrap_or("").to_string();
            // Extract the ID from user_state (it was just written by the tool)
            let us = ctx.user_state.read().unwrap();
            let todos: Vec<crate::todo::tools::TodoItem> =
                serde_json::from_value(us["__todos"].clone()).unwrap_or_default();
            let id = todos.iter().map(|t| t.id).max().unwrap_or(0);
            evs.push(DeepAgentEvent::Todo(TodoEvent::Added { id, content }));
        }
        "todo__complete" => {
            if let Some(id) = tc.arguments["id"].as_u64() {
                evs.push(DeepAgentEvent::Todo(TodoEvent::Completed { id }));
            }
        }
        "todo__update" => {
            if let (Some(id), Some(content)) = (
                tc.arguments["id"].as_u64(),
                tc.arguments["content"].as_str(),
            ) {
                evs.push(DeepAgentEvent::Todo(TodoEvent::Updated {
                    id,
                    content: content.to_string(),
                }));
            }
        }
        "todo__remove" => {
            if let Some(id) = tc.arguments["id"].as_u64() {
                evs.push(DeepAgentEvent::Todo(TodoEvent::Removed { id }));
            }
        }
        "skill__save" => {
            if let Some(name) = tc.arguments["name"].as_str() {
                // The result string contains the path
                let path = result
                    .split("saved to ")
                    .nth(1)
                    .unwrap_or("unknown")
                    .to_string();
                evs.push(DeepAgentEvent::Skill(SkillEvent::Saved {
                    name: name.to_string(),
                    path,
                }));
            }
        }
        "skill__delete" => {
            if let Some(name) = tc.arguments["name"].as_str() {
                evs.push(DeepAgentEvent::Skill(SkillEvent::Deleted {
                    name: name.to_string(),
                }));
            }
        }
        _ => {}
    }
    evs
}
