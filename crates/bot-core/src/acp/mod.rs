pub mod backend;
pub mod tools;

pub use backend::{AcpBackend, AcpLocalRunner};
pub use tools::AcpChatTool;

use std::sync::Arc;

use crate::approval::ToolApprovalManager;

pub fn register_acp_tools(
    registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry,
    backend: Arc<AcpBackend>,
    approval_manager: Arc<ToolApprovalManager>,
) {
    if backend.codex_tool_available() {
        registry.register(AcpChatTool::codex(backend, approval_manager));
    }
}
