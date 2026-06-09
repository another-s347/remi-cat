pub mod backend;
pub mod tools;

pub use backend::{AcpBackend, AcpLocalRunner};
pub use tools::AcpChatTool;

use std::sync::Arc;

pub fn register_acp_tools(
    registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry,
    backend: Arc<AcpBackend>,
) {
    registry.register(AcpChatTool::new(backend));
}
