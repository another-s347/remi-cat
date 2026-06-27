//! `bot-core` — DeepAgent re-implementation for remi-cat.
//!
//! ## Architecture
//!
//! ```text
//! CatBot
//!   └── CatAgent<AgentLoop<OpenAIClient>>
//!         ├── local_tools: search / skill__get
//!         ├── local_tools: todo__add / todo__list / todo__complete / todo__update / todo__remove
//!         └── local_tools: memory__get_detail
//!   └── MemoryStore  (shared via Arc)
//!         ├── .remi-cat/Agent.md + Soul.md  (injected as System messages every turn)
//!         ├── short_term.jsonl              (raw recent messages, per thread)
//!         ├── mid_term/<uuid>.md            (LLM-compressed, with raw archive)
//!         └── long_term/<uuid>.md           (LLM-compressed, with raw archive)
//! ```

pub mod acp;
pub mod agent;
pub mod approval;
pub mod events;
pub mod goal;
pub mod hooks;
pub mod im_tools;
pub mod markdown_runtime;
pub mod memory;
pub mod model;
pub mod model_profile;
pub mod model_usage;
pub mod profile;
pub mod remi_skill;
pub mod runtime;
pub mod sandbox;
pub mod search;
pub mod skill;
pub mod supervisor;
pub mod supervisor_workflow;
pub mod todo;
pub mod token_usage;
pub mod tool;
pub mod tool_pretty;
pub mod tools;
pub mod trigger;
pub mod user_question;

pub use agent::CatAgent;
pub use approval::{
    ApprovalResolution, ModelApprovalReviewer, ToolApprovalDecision, ToolApprovalManager,
    ToolApprovalRequest, ToolRiskLevel, ToolRiskReview,
};
pub use events::{
    CatEvent, ContextCompactionEvent, ContextCompactionSource, ContextCompactionStatus,
    ModelInputSegment, ModelInputSegmentCategory, ModelInputSnapshot, ModelInputTotals, SkillEvent,
    TodoEvent, TriggerEvent,
};
pub use goal::{GoalMaxRounds, GoalState, GoalStatus, SupervisorDecision};
pub use hooks::{HookEventName, HookManager, HookOutcome, HookPermissionDecision, HookStatus};
pub use im_tools::{ImAttachment, ImDocument, ImFileBridge};
pub use memory::{MemoryStore, ThreadHistoryMessage};
pub use model_profile::{
    api_key_from_env, install_embedded_model_profiles, model_profile_key_status,
    resolve_model_profile_from_env, validate_model_profile_api_key, ModelProfileConfig,
    ModelProfileKeyStatus, ModelProfileRegistry, ModelProfileSource, ReasoningEffort, ThinkingMode,
};
pub use model_usage::{AccountBalance, AccountUsage, AccountUsageStatus};
pub use profile::{
    embedded_agent_profile, install_embedded_agent_profiles, AgentModelBindings, AgentProfile,
    AgentRegistry,
};
pub use remi_agentloop::prelude::{Content, ContentPart, Message};
pub use runtime::{
    CatBot, CatBotBuilder, EffectiveModelProfile, EffectiveModelSource, RegisteredToolStatus,
    StreamOptions,
};
pub use skill::store::{BuiltinSkillStore, FileSkillStore};
pub use skill::store::{
    SkillDocument, SkillLoadDiagnostic, SkillLoadDiagnosticSeverity, SkillSummary,
};
pub use supervisor_workflow::{
    SupervisorTraceEvent, WorkflowDecision, WorkflowDefinition, WorkflowEdge, WorkflowInstance,
    WorkflowMaxRounds, WorkflowNode, WorkflowReport, WorkflowStatus,
};
pub use token_usage::{
    context_budget_tokens, estimate_model_input_tokens, ContextMetrics, TokenUsage,
};
pub use tool_pretty::{tool_success, PrettyToolCall, PrettyToolStatus};
pub use tools::SharedRedactor;
pub use user_question::{
    AskUserQuestionTool, UserQuestionManager, UserQuestionOption, UserQuestionRequest,
    UserQuestionResponse, UserQuestionStatus,
};

pub(crate) use runtime::{
    cat_event_from_subagent_approval_marker, metadata_flag_enabled, suppress_trigger_management,
    tool_approval_requested_marker, tool_approval_resolved_marker, tool_approval_updated_marker,
    user_question_requested_marker, user_question_resolved_marker, user_question_updated_marker,
};
