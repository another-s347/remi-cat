use std::{fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use decentralized_network::network::Address;
use decentralized_network::network::stack::{EncryptionPreference, NetStack};
use decentralized_network::transport::{UnderlayAddr, udp_raw::UdpRaw};
use once_cell::sync::{Lazy, OnceCell};
use serde::Deserialize;
use tokio::sync::Mutex;
use tonic::Code;
use tonic::Status;
use tonic::transport::Channel;
use tonic::transport::Endpoint;
use tonic_conn::{NetConnector, NetConnectorOptions};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Deserialize)]
pub struct TransportConfig {
    /// gRPC endpoint URI — required for decenet mode, optional for TCP (derived from tcpGrpcAddr).
    #[serde(default)]
    pub endpoint: String,
    /// Local virtual address — required for decenet mode, ignored for TCP.
    #[serde(default, rename = "localVirtualAddr")]
    pub local_virtual_addr: String,
    /// Remote virtual address — required for decenet mode, ignored for TCP.
    #[serde(default, rename = "remoteVirtualAddr")]
    pub remote_virtual_addr: String,
    /// Local UDP bind address — required for decenet mode, ignored for TCP.
    #[serde(default, rename = "localUdpBind")]
    pub local_udp_bind: String,
    /// Remote UDP address — required for decenet mode, ignored for TCP.
    #[serde(default, rename = "remoteUdpAddr")]
    pub remote_udp_addr: String,
    #[serde(default)]
    pub encryption: Option<String>,
    #[serde(default, rename = "introAttempts")]
    pub intro_attempts: Option<usize>,
    #[serde(default, rename = "introRetryMs")]
    pub intro_retry_ms: Option<u64>,
    #[serde(default, rename = "connectTimeoutMs")]
    pub connect_timeout_ms: Option<u64>,
    #[serde(default, rename = "requestTimeoutMs")]
    pub request_timeout_ms: Option<u64>,
    #[serde(default, rename = "keyFile")]
    pub key_file: Option<String>,
    /// "decenet" (default) or "tcp" (plain TCP gRPC).
    #[serde(default, rename = "transportMode")]
    pub transport_mode: Option<String>,
    /// Host:port for plain TCP gRPC (used when transportMode == "tcp").
    #[serde(default, rename = "tcpGrpcAddr")]
    pub tcp_grpc_addr: Option<String>,
}

pub struct TransportState {
    pub endpoint: Endpoint,
    /// `Some` = decenet (NetConnector); `None` = plain TCP.
    pub connector: Option<NetConnector>,
    pub request_timeout: Duration,
}

pub struct SharedTransport {
    state: TransportState,
    channel: Mutex<Option<Channel>>,
}

impl SharedTransport {
    pub fn request_timeout(&self) -> Duration {
        self.state.request_timeout
    }

    pub async fn get_channel(&self) -> Result<Channel, String> {
        let mut guard = self.channel.lock().await;
        if let Some(ch) = guard.as_ref() {
            tracing::debug!("[transport] get_channel: returning cached channel");
            return Ok(ch.clone());
        }

        tracing::info!("[transport] get_channel: no cached channel, creating channel...");
        let channel = if let Some(connector) = self.state.connector.clone() {
            self.state
                .endpoint
                .clone()
                .connect_with_connector(connector)
                .await
                .map_err(|err| {
                    tracing::error!("[transport] get_channel: decenet connect failed: {err}");
                    format!("Failed to connect shared transport (decenet): {err}")
                })?
        } else {
            // For direct TCP server mode, keep a lazy channel so tonic can re-establish
            // the underlying connection on the next request after transient network loss.
            self.state.endpoint.clone().connect_lazy()
        };
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
    use super::is_recoverable_transport_message;

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
}

static SHARED_TRANSPORT: OnceCell<Arc<SharedTransport>> = OnceCell::new();

/// Serializes initialization so that only one `build_transport_state` runs at a
/// time.  Without this, two concurrent callers can both pass the `OnceCell::get()`
/// fast-path, each bind a UDP socket and create a NetStack.  The loser creates a
/// zombie NetStack that has already introduced itself to the server before being
/// dropped, which appears to the server as a connection that disconnects instantly.
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
    // Serialize initialization to prevent duplicate NetStack creation.
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

pub async fn build_transport_state(config_json: &str) -> Result<TransportState, String> {
    let config: TransportConfig = serde_json::from_str(config_json)
        .map_err(|err| format!("Invalid telemetry transport config: {err}"))?;

    // TCP fast-path: skip all decenet / NetStack setup.
    if config.transport_mode.as_deref() == Some("tcp") {
        let tcp_addr = config
            .tcp_grpc_addr
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "tcpGrpcAddr is required when transportMode=tcp".to_string())?;
        let endpoint_uri = format!("http://{tcp_addr}");
        tracing::info!("[transport] build_transport_state: TCP mode, addr={tcp_addr}");
        let request_timeout = config
            .request_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
        let mut endpoint = Endpoint::from_shared(endpoint_uri)
            .map_err(|err| format!("Invalid TCP gRPC endpoint: {err}"))?;
        if let Some(timeout_ms) = config.connect_timeout_ms {
            endpoint = endpoint.connect_timeout(Duration::from_millis(timeout_ms));
        } else {
            endpoint = endpoint.connect_timeout(DEFAULT_CONNECT_TIMEOUT);
        }
        endpoint = endpoint.timeout(request_timeout);
        endpoint = endpoint.tcp_keepalive(Some(Duration::from_secs(30)));
        return Ok(TransportState {
            endpoint,
            connector: None,
            request_timeout,
        });
    }

    tracing::info!(
        "[transport] build_transport_state: localVA={}, remoteVA={}, localUDP={}, remoteUDP={}, endpoint={}",
        config.local_virtual_addr,
        config.remote_virtual_addr,
        config.local_udp_bind,
        config.remote_udp_addr,
        config.endpoint
    );

    let local_addr: Address = config.local_virtual_addr.clone().into();
    let remote_addr: Address = config.remote_virtual_addr.clone().into();

    let key_file = config
        .key_file
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_key_file_path(&config.local_virtual_addr));
    if let Some(parent) = key_file.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| {
                format!(
                    "Failed to create key file directory {}: {err}",
                    parent.display()
                )
            })?;
        }
    }

    let net = Arc::new(
        NetStack::new_with_key_file(local_addr.clone(), &key_file).map_err(|err| {
            format!(
                "Failed to initialize NetStack keys at {}: {err}",
                key_file.display()
            )
        })?,
    );
    if let Some(mode) = config.encryption.as_deref() {
        match mode.to_ascii_lowercase().as_str() {
            "plaintext" => net.set_encryption_preference(EncryptionPreference::Plaintext),
            "encrypted" | "tls" => {
                net.set_encryption_preference(EncryptionPreference::ForceEncrypted)
            }
            _ => {}
        }
    }

    let local_udp: SocketAddr = config
        .local_udp_bind
        .parse()
        .map_err(|err| format!("Invalid local UDP bind address: {err}"))?;
    let udp = Arc::new(
        UdpRaw::bind(local_udp)
            .await
            .map_err(|err| format!("Failed to bind local UDP socket: {err}"))?,
    );
    net.add_transport(udp.clone())
        .await
        .map_err(|err| format!("Failed to add UDP transport: {err}"))?;
    tracing::info!(
        "[transport] build_transport_state: add_transport done (recv pump spawned by decenet)"
    );

    let remote_udp: SocketAddr = config
        .remote_udp_addr
        .parse()
        .map_err(|err| format!("Invalid remote UDP address: {err}"))?;
    net.add_neighbor(remote_addr, UnderlayAddr::Udp(remote_udp), udp.clone())
        .await;
    tracing::info!("[transport] build_transport_state: add_neighbor done, remote={remote_udp}");

    let mut connector =
        NetConnector::new(net.clone(), local_addr).with_options(NetConnectorOptions {
            reintroduce_on_connect: true,
        });
    if config.intro_attempts.is_some() || config.intro_retry_ms.is_some() {
        let attempts = config.intro_attempts.unwrap_or(32);
        let delay = Duration::from_millis(config.intro_retry_ms.unwrap_or(200));
        connector = connector.with_retry(attempts, delay);
    }

    let endpoint_uri = format!("http://{}", config.endpoint);

    let mut endpoint = Endpoint::from_shared(endpoint_uri)
        .map_err(|err| format!("Invalid endpoint URI: {err}"))?;
    let request_timeout = config
        .request_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT);
    if let Some(timeout_ms) = config.connect_timeout_ms {
        endpoint = endpoint.connect_timeout(Duration::from_millis(timeout_ms));
    } else {
        endpoint = endpoint.connect_timeout(DEFAULT_CONNECT_TIMEOUT);
    }
    endpoint = endpoint.timeout(request_timeout);
    endpoint = endpoint.tcp_keepalive(Some(Duration::from_secs(30)));

    Ok(TransportState {
        endpoint,
        connector: Some(connector),
        request_timeout,
    })
}

fn fallback_key_file_path(local_addr: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let sanitized = local_addr
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    path.push(format!("telemetry_netstack_{sanitized}.keys"));
    path
}
