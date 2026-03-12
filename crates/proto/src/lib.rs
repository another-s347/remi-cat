//! Generated protobuf/gRPC types for remi Daemon ↔ Agent communication.

pub mod remi {
    tonic::include_proto!("remi");
}

pub use remi::agent_message::Payload as AgentPayload;
pub use remi::daemon_message::Payload as DaemonPayload;
pub use remi::daemon_service_client::DaemonServiceClient;
pub use remi::daemon_service_server::{DaemonService, DaemonServiceServer};
pub use remi::*;
