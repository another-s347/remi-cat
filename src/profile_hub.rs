use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use a2a::{SendMessageRequest, StreamResponse};
use anyhow::{Context, Result};
use bytes::Bytes;
use http::{header, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::OnceCell;
use weaver_core::{AppAddr, ClientAddr, DeviceId};
use weaver_crypto::NetworkRootPublic;
use weaver_net::{LocalBinding, NetworkHandle, NetworkHandleTransportOptions};
use zeroize::Zeroizing;

use crate::runtime_config::{ProfileHubConfig, RuntimeConfig, WeaverNetworkConfig};
use crate::weaver_http::{
    http1_client, production_open_options, WeaverHttpClient, WeaverHttpConnector,
};

#[derive(Clone)]
pub(crate) struct ProfileHubClient {
    transport: ProfileHubTransport,
    id: String,
    base_url: String,
    token: String,
    timeout: Duration,
}

#[derive(Clone)]
enum ProfileHubTransport {
    Weaver(Arc<WeaverNetworkRuntime>),
    #[cfg(test)]
    Tcp(reqwest::Client),
}

struct WeaverNetworkRuntime {
    config: WeaverNetworkConfig,
    config_dir: PathBuf,
    client: OnceCell<WeaverHttpClient>,
}

impl WeaverNetworkRuntime {
    fn new(config: WeaverNetworkConfig, config_dir: PathBuf) -> Self {
        Self {
            config,
            config_dir,
            client: OnceCell::new(),
        }
    }

    async fn client(&self) -> Result<&WeaverHttpClient> {
        self.client
            .get_or_try_init(|| async {
                let data_dir = resolve_path(&self.config_dir, &self.config.data_dir);
                let master_key_file = resolve_path(&self.config_dir, &self.config.master_key_file);
                let key = Zeroizing::new(std::fs::read(&master_key_file).with_context(|| {
                    format!("reading Weaver master key {}", master_key_file.display())
                })?);
                let master_key: [u8; 32] = key
                    .as_slice()
                    .try_into()
                    .context("Weaver master key must contain exactly 32 bytes")?;
                let root_bytes = hex::decode(&self.config.root_public_key)
                    .context("decoding Weaver root public key")?;
                let root_bytes: [u8; 32] = root_bytes.try_into().map_err(|_| {
                    anyhow::anyhow!("Weaver root public key must contain exactly 32 bytes")
                })?;
                let root = NetworkRootPublic::from_bytes(&root_bytes)?;
                let app_addr: AppAddr = self.config.app_addr.parse()?;
                let device_id: DeviceId = self.config.device_id.parse()?;
                let options = production_open_options(root, &data_dir, master_key)?;
                let source = ClientAddr::new(app_addr, device_id);
                let network = Arc::new(
                    NetworkHandle::open_with_transport_options(
                        options,
                        [LocalBinding::Client(source)],
                        NetworkHandleTransportOptions {
                            disable_direct_paths: self.config.relay_only,
                        },
                    )
                    .await
                    .with_context(|| format!("opening Weaver network `{}`", self.config.id))?,
                );
                Ok(http1_client(WeaverHttpConnector::new(network, source)))
            })
            .await
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct RemoteProfile {
    pub hub_profile_id: String,
    pub device_id: String,
    pub profile_id: String,
    pub alias: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub version: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub intents: Vec<String>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct ListResponse<T> {
    items: Vec<T>,
}

impl ProfileHubClient {
    fn from_config(
        config: &ProfileHubConfig,
        networks: &HashMap<String, Arc<WeaverNetworkRuntime>>,
    ) -> Result<Option<Self>> {
        config.validate()?;
        if !config.enabled {
            return Ok(None);
        }
        let token = std::env::var(&config.token_env).with_context(|| {
            format!(
                "Profile Hub is enabled but secret `{}` is unavailable",
                config.token_env
            )
        })?;
        if token.trim().is_empty() {
            anyhow::bail!(
                "Profile Hub is enabled but secret `{}` is empty",
                config.token_env
            );
        }
        let network = networks.get(&config.weaver_network).with_context(|| {
            format!(
                "Profile Hub `{}` references missing Weaver network `{}`",
                config.id, config.weaver_network
            )
        })?;
        Ok(Some(Self {
            transport: ProfileHubTransport::Weaver(network.clone()),
            id: config.id.clone(),
            base_url: config.url.trim_end_matches('/').to_string(),
            token,
            timeout: Duration::from_millis(config.request_timeout_ms),
        }))
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        id: impl Into<String>,
        base_url: String,
        token: impl Into<String>,
    ) -> Self {
        Self {
            transport: ProfileHubTransport::Tcp(reqwest::Client::new()),
            id: id.into(),
            base_url,
            token: token.into(),
            timeout: Duration::from_secs(5),
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) async fn discover(
        &self,
        tags: &[String],
        intents: &[String],
    ) -> Result<Vec<RemoteProfile>> {
        let mut url = reqwest::Url::parse(&format!("{}/api/v1/profiles", self.base_url))?;
        url.query_pairs_mut().append_pair("status", "online");
        let tags_value = (!tags.is_empty()).then(|| tags.join(","));
        let intents_value = (!intents.is_empty()).then(|| intents.join(","));
        if let Some(tags) = tags_value.as_deref() {
            url.query_pairs_mut().append_pair("tags", tags);
        }
        if let Some(intents) = intents_value.as_deref() {
            url.query_pairs_mut().append_pair("intents", intents);
        }
        let (status, bytes) = match &self.transport {
            ProfileHubTransport::Weaver(network) => {
                let request = Request::builder()
                    .method(http::Method::GET)
                    .uri(url.as_str())
                    .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
                    .body(Full::new(Bytes::new()))?;
                let response =
                    tokio::time::timeout(self.timeout, network.client().await?.request(request))
                        .await
                        .context("Profile Hub discovery timed out")?
                        .context("querying Profile Hub over Weaver")?;
                let status = response.status();
                let bytes = response.into_body().collect().await?.to_bytes();
                (status, bytes)
            }
            #[cfg(test)]
            ProfileHubTransport::Tcp(client) => {
                let response = client
                    .get(url)
                    .bearer_auth(&self.token)
                    .timeout(self.timeout)
                    .send()
                    .await
                    .context("querying Profile Hub test server")?;
                let status = response.status();
                let bytes = response.bytes().await?;
                (status, bytes)
            }
        };
        if !status.is_success() {
            anyhow::bail!(
                "Profile Hub discovery returned HTTP {status}: {}",
                String::from_utf8_lossy(&bytes).trim()
            );
        }
        Ok(
            serde_json::from_slice::<ListResponse<RemoteProfile>>(&bytes)
                .context("decoding Profile Hub discovery response")?
                .items,
        )
    }

    pub(crate) async fn invoke(
        &self,
        hub_profile_id: &str,
        request: &SendMessageRequest,
    ) -> Result<Vec<StreamResponse>> {
        if hub_profile_id.is_empty() || hub_profile_id.contains('/') {
            anyhow::bail!("invalid Profile Hub profile reference");
        }
        let card_path = format!("/a2a/{hub_profile_id}/.well-known/agent-card.json");
        let (status, bytes) = self
            .request(Method::GET, &card_path, Bytes::new(), None)
            .await
            .context("discovering remote A2A Agent Card")?;
        ensure_success(status, &bytes, "Agent Card")?;
        let card: Value =
            serde_json::from_slice(&bytes).context("decoding remote A2A Agent Card")?;
        let streaming = card
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("streaming"))
            .and_then(Value::as_bool);
        if streaming != Some(true) {
            anyhow::bail!("remote A2A profile does not advertise streaming");
        }

        let stream_path = format!("/a2a/{hub_profile_id}/message:stream");
        let body = serde_json::to_vec(request).context("encoding remote A2A request")?;
        let (status, bytes) = self
            .request(
                Method::POST,
                &stream_path,
                Bytes::from(body),
                Some("application/a2a+json"),
            )
            .await
            .context("streaming remote A2A response")?;
        ensure_success(status, &bytes, "message stream")?;
        parse_sse_events(&bytes)
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        content_type: Option<&str>,
    ) -> Result<(StatusCode, Bytes)> {
        let url = format!("{}{}", self.base_url, path);
        match &self.transport {
            ProfileHubTransport::Weaver(network) => {
                let mut builder = Request::builder()
                    .method(method)
                    .uri(&url)
                    .header(header::AUTHORIZATION, format!("Bearer {}", self.token));
                if let Some(content_type) = content_type {
                    builder = builder.header(header::CONTENT_TYPE, content_type);
                }
                let request = builder.body(Full::new(body))?;
                let response =
                    tokio::time::timeout(self.timeout, network.client().await?.request(request))
                        .await
                        .context("Profile Hub request timed out")?
                        .context("querying Profile Hub over Weaver")?;
                let status = response.status();
                let bytes = tokio::time::timeout(
                    Duration::from_secs(5 * 60),
                    response.into_body().collect(),
                )
                .await
                .context("Profile Hub response body timed out")??
                .to_bytes();
                Ok((status, bytes))
            }
            #[cfg(test)]
            ProfileHubTransport::Tcp(client) => {
                let mut request = client
                    .request(method, url)
                    .bearer_auth(&self.token)
                    .body(body);
                if let Some(content_type) = content_type {
                    request = request.header(header::CONTENT_TYPE, content_type);
                }
                let response = request
                    .timeout(Duration::from_secs(5 * 60))
                    .send()
                    .await
                    .context("querying Profile Hub test server")?;
                let status = response.status();
                let bytes = response.bytes().await?;
                Ok((status, bytes))
            }
        }
    }
}

fn ensure_success(status: StatusCode, body: &[u8], phase: &str) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    anyhow::bail!(
        "remote A2A {phase} returned HTTP {status}: {}",
        String::from_utf8_lossy(body).trim()
    )
}

fn parse_sse_events(body: &[u8]) -> Result<Vec<StreamResponse>> {
    let text = String::from_utf8(body.to_vec()).context("remote A2A stream is not UTF-8")?;
    let normalized = text.replace("\r\n", "\n");
    let mut events = Vec::new();
    for frame in normalized.split("\n\n") {
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        events.push(
            serde_json::from_str(&data)
                .with_context(|| format!("decoding remote A2A SSE event `{data}`"))?,
        );
    }
    if events.is_empty() {
        anyhow::bail!("remote A2A stream completed without events");
    }
    Ok(events)
}

pub(crate) fn clients_from_runtime(
    config: &RuntimeConfig,
    config_path: &Path,
) -> Result<Vec<ProfileHubClient>> {
    let config_dir = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let networks = config
        .weaver_networks
        .iter()
        .cloned()
        .map(|network| {
            let id = network.id.clone();
            (
                id,
                Arc::new(WeaverNetworkRuntime::new(network, config_dir.clone())),
            )
        })
        .collect::<HashMap<_, _>>();
    config
        .profile_hubs
        .iter()
        .map(|hub| ProfileHubClient::from_config(hub, &networks))
        .collect::<Result<Vec<_>>>()
        .map(|clients| clients.into_iter().flatten().collect())
}

fn resolve_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

impl RemoteProfile {
    pub(crate) fn reference(&self, hub_id: &str) -> String {
        format!("hub:{hub_id}/{}", self.hub_profile_id)
    }

    pub(crate) fn tool_value(&self, hub_id: &str) -> Value {
        json!({
            "reference": self.reference(hub_id),
            "source": "profile_hub",
            "hub_id": hub_id,
            "hub_profile_id": self.hub_profile_id,
            "device_id": self.device_id,
            "id": self.profile_id,
            "alias": self.alias,
            "name": self.name,
            "description": self.description,
            "version": self.version,
            "endpoint_type": "remote",
            "status": self.status,
            "tags": self.tags,
            "intents": self.intents,
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::extract::Query;
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::{json, Value};

    use super::*;

    #[tokio::test]
    async fn discovers_online_profiles_with_auth_and_filters() {
        async fn list(headers: HeaderMap, Query(query): Query<Value>) -> (StatusCode, Json<Value>) {
            if headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                != Some("Bearer hub-token")
            {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error":"unauthorized"})),
                );
            }
            assert_eq!(query["status"], "online");
            assert_eq!(query["tags"], "travel,tools");
            assert_eq!(query["intents"], "plan-trip");
            (
                StatusCode::OK,
                Json(json!({"items":[{
                    "hub_profile_id":"hub-profile-1",
                    "device_id":"device-1",
                    "profile_id":"travel",
                    "alias":"travel",
                    "name":"Travel Planner",
                    "description":"Plans trips",
                    "version":"1.0.0",
                    "tags":["travel","tools"],
                    "intents":["plan-trip"],
                    "status":"online"
                }]})),
            )
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/api/v1/profiles", get(list)))
                .await
                .unwrap();
        });
        let client = ProfileHubClient::for_test("office", format!("http://{address}"), "hub-token");
        let profiles = client
            .discover(
                &["travel".to_string(), "tools".to_string()],
                &["plan-trip".to_string()],
            )
            .await
            .unwrap();

        assert_eq!(profiles.len(), 1);
        assert_eq!(
            profiles[0].reference(client.id()),
            "hub:office/hub-profile-1"
        );
        assert_eq!(profiles[0].tool_value(client.id())["source"], "profile_hub");
    }

    #[tokio::test]
    async fn rejects_invalid_hub_token() {
        let app = Router::new().route(
            "/api/v1/profiles",
            get(|| async { (StatusCode::UNAUTHORIZED, "no") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let error = ProfileHubClient::for_test("office", format!("http://{address}"), "wrong")
            .discover(&[], &[])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("HTTP 401"));
    }
}
