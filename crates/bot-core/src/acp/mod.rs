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
    if backend.active_tool_available() {
        let name = backend.active_tool_name();
        let description = backend.active_tool_description();
        registry.register(AcpChatTool::new(
            backend,
            approval_manager,
            name,
            description,
        ));
    }
}
