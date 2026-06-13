use std::{sync::Arc, time::Duration};

use once_cell::sync::{Lazy, OnceCell};
use serde::Deserialize;
use tokio::sync::Mutex;
use tonic::Code;
use tonic::Status;
use tonic::transport::Channel;
use tonic::transport::Endpoint;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub const OFFLINE_TRANSPORT_ERROR: &str =
    "Remi SDK is running in offline mode; remote transport is unavailable";

#[derive(Debug, Clone, Deserialize)]
pub struct TransportConfig {
    #[serde(default, rename = "connectTimeoutMs")]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default, rename = "requestTimeoutMs")]
    pub request_timeout_ms: Option<u64>,
    /// "tcp" (plain TCP gRPC) or "offline".
    #[serde(default, rename = "transportMode")]
    pub transport_mode: Option<String>,
    /// Host:port for plain TCP gRPC.
    #[serde(default, rename = "tcpGrpcAddr")]
    pub tcp_grpc_addr: Option<String>,
}

pub struct TransportState {
    pub mode: SharedTransportMode,
    pub endpoint: Option<Endpoint>,
    pub request_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedTransportMode {
    Offline,
    Tcp,
}

pub struct SharedTransport {
    state: TransportState,
    channel: Mutex<Option<Channel>>,
}

impl SharedTransport {
    pub fn request_timeout(&self) -> Duration {
        self.state.request_timeout
    }

    pub fn mode(&self) -> SharedTransportMode {
        self.state.mode
    }

    pub fn is_offline(&self) -> bool {
        self.state.mode == SharedTransportMode::Offline
    }

    pub async fn get_channel(&self) -> Result<Channel, String> {
        if self.is_offline() {
            tracing::debug!("[transport] get_channel: offline transport has no remote channel");
            return Err(OFFLINE_TRANSPORT_ERROR.to_string());
        }

        let mut guard = self.channel.lock().await;
        if let Some(ch) = guard.as_ref() {
            tracing::debug!("[transport] get_channel: returning cached channel");
            return Ok(ch.clone());
        }

        tracing::info!("[transport] get_channel: no cached channel, creating channel...");
        let endpoint = self
            .state
            .endpoint
            .as_ref()
            .ok_or_else(|| "Shared transport endpoint is not configured".to_string())?;
        // Keep a lazy channel so tonic can re-establish the underlying
        // connection on the next request after transient network loss.
        let channel = endpoint.clone().connect_lazy();
        tracing::info!("[transport] get_channel: channel ready");
        guard.replace(channel.clone());

        Ok(channel)
    }

    /// Invalidate the cached channel so the next `get_channel` reconnects.
    pub async fn invalidate_channel(&self) {
        let mut guard = self.channel.lock().await;
        if guard.take().is_some() {
            tracing::info!(
                "[transport] invalidate_channel: cached channel dropped, will reconnect on next use"
            );
        }
    }
}

pub fn is_recoverable_transport_status(status: &Status) -> bool {
    matches!(
        status.code(),
        Code::Unavailable | Code::Cancelled | Code::Unknown | Code::DeadlineExceeded
    ) || is_recoverable_transport_message(status.message())
}

pub fn is_recoverable_transport_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();

    [
        "connection reset",
        "broken pipe",
        "connection refused",
        "connection aborted",
        "timed out",
        "deadline has elapsed",
        "transport error",
        "tcp connect error",
        "dns error",
        "network unreachable",
        "temporarily unavailable",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::{
        OFFLINE_TRANSPORT_ERROR, SharedTransport, SharedTransportMode, build_transport_state,
        is_offline_transport_config, is_recoverable_transport_message,
    };
    use serde_json::json;
    use tokio::sync::Mutex;

    #[test]
    fn detects_connection_reset_messages() {
        assert!(is_recoverable_transport_message(
            "transport error: connection reset by peer"
        ));
        assert!(is_recoverable_transport_message(
            "deadline has elapsed while waiting for response"
        ));
        assert!(!is_recoverable_transport_message("permission denied"));
    }

    #[test]
    fn detects_explicit_offline_transport_config() {
        let config = serde_json::from_value(json!({
            "transportMode": "offline"
        }))
        .expect("valid config");

        assert!(is_offline_transport_config(&config));
    }

    #[test]
    fn detects_tcp_without_remote_address_as_offline() {
        let config = serde_json::from_value(json!({
            "transportMode": "tcp",
            "tcpGrpcAddr": "  "
        }))
        .expect("valid config");

        assert!(is_offline_transport_config(&config));
    }

    #[test]
    fn detects_unknown_transport_mode_as_offline() {
        let config = serde_json::from_value(json!({
            "transportMode": "unsupported",
            "tcpGrpcAddr": "127.0.0.1:50051"
        }))
        .expect("valid config");

        assert!(is_offline_transport_config(&config));
    }

    #[test]
    fn accepts_valid_tcp_remote_config() {
        let config = serde_json::from_value(json!({
            "transportMode": "tcp",
            "tcpGrpcAddr": "127.0.0.1:50051"
        }))
        .expect("valid config");

        assert!(!is_offline_transport_config(&config));
    }

    #[tokio::test]
    async fn offline_transport_get_channel_fails_without_network_setup() {
        let state = build_transport_state(
            &json!({
                "transportMode": "offline",
                "requestTimeoutMs": 1234
            })
            .to_string(),
        )
        .await
        .expect("offline transport state should build");

        assert_eq!(state.mode, SharedTransportMode::Offline);
        assert!(state.endpoint.is_none());

        let transport = SharedTransport {
            state,
            channel: Mutex::new(None),
        };

        let error = transport
            .get_channel()
            .await
            .expect_err("offline transport should not create a channel");
        assert_eq!(error, OFFLINE_TRANSPORT_ERROR);
    }
}

static SHARED_TRANSPORT: OnceCell<Arc<SharedTransport>> = OnceCell::new();

/// Serializes initialization so that only one `build_transport_state` runs at a
/// time. Without this, two concurrent callers can both pass the `OnceCell::get()`
/// fast-path and race to configure the shared transport.
static TRANSPORT_INIT_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

pub async fn configure_shared_transport(config_json: &str) -> Result<Arc<SharedTransport>, String> {
    // Fast path — already configured.
    if let Some(existing) = SHARED_TRANSPORT.get() {
        tracing::info!(
            "[transport] configure_shared_transport: fast-path reuse (already configured)"
        );
        return Ok(existing.clone());
    }

    tracing::info!("[transport] configure_shared_transport: first call, acquiring init lock...");
    // Serialize initialization to prevent duplicate shared transport creation.
    let _lock = TRANSPORT_INIT_LOCK.lock().await;

    // Double-check after acquiring the lock.
    if let Some(existing) = SHARED_TRANSPORT.get() {
        tracing::info!(
            "[transport] configure_shared_transport: double-check hit, reusing existing"
        );
        return Ok(existing.clone());
    }

    tracing::info!("[transport] configure_shared_transport: building new transport state...");
    let state = build_transport_state(config_json).await?;
    let shared = Arc::new(SharedTransport {
        state,
        channel: Mutex::new(None),
    });

    let _ = SHARED_TRANSPORT.set(shared.clone());
    tracing::info!(
        "[transport] configure_shared_transport: transport created and stored in OnceCell"
    );
    Ok(SHARED_TRANSPORT.get().cloned().unwrap_or(shared))
}

pub fn get_shared_transport() -> Result<Arc<SharedTransport>, String> {
    SHARED_TRANSPORT
        .get()
        .cloned()
        .ok_or_else(|| "Shared transport is not configured".to_string())
}

pub fn is_shared_transport_offline() -> bool {
    SHARED_TRANSPORT
        .get()
        .map(|transport| transport.is_offline())
        .unwrap_or(false)
}

pub async fn build_transport_state(config_json: &str) -> Result<TransportState, String> {
    let config: TransportConfig = serde_json::from_str(config_json)
        .map_err(|err| format!("Invalid telemetry transport config: {err}"))?;

    let request_timeout = config
        .request_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT);

    if is_offline_transport_config(&config) {
        tracing::info!("[transport] build_transport_state: offline mode, skipping network setup");
        return Ok(TransportState {
            mode: SharedTransportMode::Offline,
            endpoint: None,
            request_timeout,
        });
    }

    let tcp_addr = config
        .tcp_grpc_addr
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "tcpGrpcAddr is required for remote transport".to_string())?;
    let endpoint_uri = format!("http://{tcp_addr}");
    tracing::info!("[transport] build_transport_state: TCP mode, addr={tcp_addr}");
    let mut endpoint = Endpoint::from_shared(endpoint_uri)
        .map_err(|err| format!("Invalid TCP gRPC endpoint: {err}"))?;
    if let Some(timeout_ms) = config.connect_timeout_ms {
        endpoint = endpoint.connect_timeout(Duration::from_millis(timeout_ms));
    } else {
        endpoint = endpoint.connect_timeout(DEFAULT_CONNECT_TIMEOUT);
    }
    endpoint = endpoint.timeout(request_timeout);
    endpoint = endpoint.tcp_keepalive(Some(Duration::from_secs(30)));

    Ok(TransportState {
        mode: SharedTransportMode::Tcp,
        endpoint: Some(endpoint),
        request_timeout,
    })
}

pub fn is_offline_transport_config(config: &TransportConfig) -> bool {
    match config.transport_mode.as_deref().map(str::trim) {
        Some(mode) if mode.eq_ignore_ascii_case("offline") || mode.eq_ignore_ascii_case("none") => {
            true
        }
        Some(mode) if !mode.eq_ignore_ascii_case("tcp") => true,
        _ => config
            .tcp_grpc_addr
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none(),
    }
}
