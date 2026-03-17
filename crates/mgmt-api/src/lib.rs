//! `mgmt-api` — Management protocol types shared between `remi-daemon` and `remi-admin`.
//!
//! All management communication uses JSON request/response messages that are
//! transported over an encrypted Noise_XX WebSocket channel.
//!
//! # Protocol flow
//!
//! 1. Client opens a WebSocket connection to `daemon:50052`.
//! 2. Both sides perform a Noise_XX handshake (3 binary WS frames).
//! 3. First JSON message from client **must** be `method = "auth"`.
//!    - If the client's public key is already trusted: token may be empty.
//!    - If the client is new: provide the daemon's `mgmt_token` to pair.
//! 4. Subsequent messages: normal request/response.
//!
//! # Envelope
//!
//! ```json
//! { "id": "abc", "method": "daemon.status", "params": {} }
//! { "id": "abc", "result": { ... } }
//! { "id": "abc", "error": { "code": 403, "message": "..." } }
//! ```

use serde::{Deserialize, Serialize};

// ── Wire envelope ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MgmtRequest {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MgmtResponse {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<MgmtError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MgmtError {
    pub code: i32,
    pub message: String,
}

impl MgmtResponse {
    pub fn ok(id: impl Into<String>, result: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: impl Into<String>, code: i32, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            result: None,
            error: Some(MgmtError {
                code,
                message: message.into(),
            }),
        }
    }

    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

// ── Method name constants ─────────────────────────────────────────────────────

pub mod methods {
    /// First request after handshake — authenticate/pair the admin client.
    pub const AUTH: &str = "auth";
    /// Daemon status: agent connection, owner, uptime.
    pub const DAEMON_STATUS: &str = "daemon.status";
    /// Container lifecycle: restart | stop | start | pull | recreate.
    pub const CONTAINER_OP: &str = "container.op";
    /// Owner info.
    pub const OWNER_GET: &str = "owner.get";
    /// Reset owner (clear owner binding).
    pub const OWNER_RESET: &str = "owner.reset";
    /// List secret keys.
    pub const SECRET_LIST: &str = "secret.list";
    /// Set a secret key/value.
    pub const SECRET_SET: &str = "secret.set";
    /// Delete a secret key.
    pub const SECRET_DELETE: &str = "secret.delete";
    /// Read an agent data file (Agent.md / Soul.md).
    pub const AGENT_FILE_READ: &str = "agent_file.read";
    /// Write an agent data file.
    pub const AGENT_FILE_WRITE: &str = "agent_file.write";
    /// List configured volume bind mounts.
    pub const VOLUME_LIST: &str = "volume.list";
    /// Add a volume bind mount configuration.
    pub const VOLUME_ADD: &str = "volume.add";
    /// Remove a volume bind mount configuration.
    pub const VOLUME_REMOVE: &str = "volume.remove";
    /// List all users (UUID + channel identities).
    pub const USER_LIST: &str = "user.list";
    /// Link two channel identities to the same user UUID.
    pub const USER_LINK: &str = "user.link";
    /// Unlink a channel identity from its user.
    pub const USER_UNLINK: &str = "user.unlink";
    /// Delete a user by UUID (removes all their channel mappings).
    pub const USER_DELETE: &str = "user.delete";
    /// Add a UUID to the blacklist.
    pub const USER_BAN: &str = "user.ban";
    /// Remove a UUID from the blacklist.
    pub const USER_UNBAN: &str = "user.unban";
    /// List blacklisted UUIDs.
    pub const USER_BAN_LIST: &str = "user.ban_list";
}

// ── Typed params / results ────────────────────────────────────────────────────

/// `auth` params — sent as first message by admin client.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AuthParams {
    /// Mgmt token — required for initial pairing; omit (or pass "") if already paired.
    #[serde(default)]
    pub token: String,
}

/// `auth` result.
#[derive(Debug, Serialize, Deserialize)]
pub struct AuthResult {
    pub ok: bool,
    /// `true` when this was a fresh pairing (public key newly registered).
    #[serde(default)]
    pub paired: bool,
    pub daemon_version: String,
}

/// `daemon.status` result.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonStatusResult {
    pub agent_connected: bool,
    pub owner_id: Option<String>,
    pub uptime_secs: u64,
    pub daemon_version: String,
    pub container_running: Option<bool>,
    /// Daemon process RSS in KiB.
    pub daemon_mem_kb: u64,
    /// Daemon CPU usage % (instantaneous sample; 0 on first poll).
    pub daemon_cpu_pct: f32,
    /// Agent process RSS in KiB, if the process was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_mem_kb: Option<u64>,
    /// Agent CPU usage %, if the process was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_cpu_pct: Option<f32>,
}

/// `container.op` params.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContainerOpParams {
    /// `"restart"` | `"stop"` | `"start"` | `"pull"`
    pub op: String,
}

/// `container.op` result.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContainerOpResult {
    pub ok: bool,
    pub message: String,
}

/// `owner.get` result.
#[derive(Debug, Serialize, Deserialize)]
pub struct OwnerGetResult {
    pub owner_id: Option<String>,
}

/// Entry in the secret list.
#[derive(Debug, Serialize, Deserialize)]
pub struct SecretEntry {
    pub key: String,
}

/// `secret.set` params.
#[derive(Debug, Serialize, Deserialize)]
pub struct SecretSetParams {
    pub key: String,
    pub value: String,
}

/// `secret.delete` params.
#[derive(Debug, Serialize, Deserialize)]
pub struct SecretDeleteParams {
    pub key: String,
}

/// `agent_file.read` / `agent_file.write` params.
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentFileParams {
    /// Filename: `"Agent.md"` or `"Soul.md"`.
    pub filename: String,
    /// Content — populated for write requests.
    #[serde(default)]
    pub content: String,
}

/// `agent_file.read` result.
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentFileResult {
    pub content: String,
}

/// A single volume bind mount: maps a host directory into the container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeMount {
    /// Absolute path on the **host** machine.
    pub host_path: String,
    /// Absolute path inside the container.
    pub container_path: String,
    /// When `true` the mount is read-only.
    #[serde(default)]
    pub read_only: bool,
}

/// `volume.add` params.
#[derive(Debug, Serialize, Deserialize)]
pub struct VolumeAddParams {
    pub host_path: String,
    pub container_path: String,
    #[serde(default)]
    pub read_only: bool,
}

/// `volume.remove` params — identify the mount by its container path.
#[derive(Debug, Serialize, Deserialize)]
pub struct VolumeRemoveParams {
    pub container_path: String,
}
// ── User management types ─────────────────────────────────────────────────────

/// A single channel identity (channel name + channel-specific user ID).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserChannel {
    pub channel: String,
    pub user_id: String,
}

/// A user record returned by `user.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub uuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub channels: Vec<UserChannel>,
    #[serde(default)]
    pub banned: bool,
}

/// `user.list` result.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserListResult {
    pub users: Vec<UserInfo>,
}

/// `user.link` params — link two channel identities to one UUID.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserLinkParams {
    pub channel_a: String,
    pub user_id_a: String,
    pub channel_b: String,
    pub user_id_b: String,
}

/// `user.link` result.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserLinkResult {
    pub uuid: String,
}

/// `user.unlink` params — remove a channel identity from its user.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserUnlinkParams {
    pub channel: String,
    pub user_id: String,
}

/// `user.delete` params — delete a user by UUID.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserDeleteParams {
    pub uuid: String,
}

/// `user.ban` / `user.unban` params.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserBanParams {
    pub uuid: String,
}

/// `user.ban_list` result.
#[derive(Debug, Serialize, Deserialize)]
pub struct UserBanListResult {
    pub users: Vec<String>,
}
