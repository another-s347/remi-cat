//! Tool registry, approval, and built-in tool building blocks.
//!
//! Runtime code should depend on these abstractions instead of knowing about a
//! concrete channel. Channel-specific tools live behind their own tool
//! registration APIs.

pub use crate::approval::{
    ApprovalResolution, ToolApprovalDecision, ToolApprovalManager, ToolApprovalRequest,
    ToolRiskLevel, ToolRiskReview,
};
pub use crate::im_tools::{ImAttachment, ImDocument, ImFileBridge};
pub use crate::search::SearchTool;
pub use crate::tool_pretty::{tool_success, PrettyToolCall, PrettyToolStatus};
pub use crate::tools::{
    BashMode, ExaSearchTool, ManageYourselfTool, NowTool, RootedFsApplyPatchTool,
    RootedFsCreateTool, RootedFsLsTool, RootedFsReadTool, RootedFsRemoveTool, RootedFsWriteTool,
    SecretRedactor, SharedRedactor, SleepTool, WorkspaceBashTool, WorkspaceSshTool,
};
pub use crate::user_question::{
    AskUserQuestionTool, UserQuestionManager, UserQuestionOption, UserQuestionRequest,
    UserQuestionResponse, UserQuestionStatus,
};
