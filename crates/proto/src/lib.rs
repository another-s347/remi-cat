//! Generated protobuf/gRPC types for remi Daemon ↔ Agent communication.

/// Large IM image payloads and daemon-mediated file bridge messages can exceed
/// tonic's default 4 MiB decode limit.
pub const GRPC_MESSAGE_LIMIT_BYTES: usize = 64 * 1024 * 1024;

pub mod remi {
    tonic::include_proto!("remi");
}

pub use remi::agent_message::Payload as AgentPayload;
pub use remi::daemon_message::Payload as DaemonPayload;
pub use remi::daemon_service_client::DaemonServiceClient;
pub use remi::daemon_service_server::{DaemonService, DaemonServiceServer};
pub use remi::*;
