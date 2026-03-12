//! Axum HTTP handlers — proxy requests to the appropriate daemon via Noise.
//!
//! All `/api/daemons/:id/*` routes look up the daemon in the registry,
//! build a `MgmtRequest`, call the daemon through the `NoiseClient`, and
//! return the result as JSON.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use mgmt_api::{
    methods, AgentFileParams, ContainerOpParams, MgmtRequest, SecretDeleteParams, SecretSetParams,
};
use serde::{Deserialize};
use serde_json::json;
use tokio::sync::RwLock;

use crate::noise_client::NoiseClient;
use crate::registry::Registry;

// ── Shared application state ──────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<RwLock<Registry>>,
    pub noise: Arc<NoiseClient>,
}

// ── Helper: call daemon ───────────────────────────────────────────────────────

/// Look up daemon, call it, return `MgmtResponse` or 4xx/5xx.
async fn call(
    state: &AppState,
    daemon_id: &str,
    req: MgmtRequest,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let entry = {
        let reg = state.registry.read().await;
        reg.get(daemon_id)
            .ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "daemon not found"})),
                )
            })?
            .clone()
    };

    let result = state
        .noise
        .call(&entry.addr, entry.fingerprint.as_deref(), "", req)
        .await;

    match result {
        Err(e) => Err((
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        )),
        Ok((resp, fingerprint)) => {
            // Persist fingerprint on first successful contact
            if entry.fingerprint.is_none() {
                let mut reg = state.registry.write().await;
                let _ = reg.set_fingerprint(daemon_id, fingerprint);
            }
            if let Some(err) = resp.error {
                Err((
                    StatusCode::from_u16(err.code as u16)
                        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                    Json(json!({"error": err.message})),
                ))
            } else {
                Ok(resp.result.unwrap_or(serde_json::Value::Null))
            }
        }
    }
}

macro_rules! try_call {
    ($state:expr, $id:expr, $req:expr) => {
        match call($state, $id, $req).await {
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err((code, body)) => (code, body).into_response(),
        }
    };
}

// ── Daemon list ───────────────────────────────────────────────────────────────

pub async fn list_daemons(State(s): State<AppState>) -> impl IntoResponse {
    let reg = s.registry.read().await;
    Json(json!(reg.daemons))
}

// ── Add daemon ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AddDaemonBody {
    pub label: String,
    pub addr: String,
    /// Management token (for initial pairing).
    pub token: String,
}

pub async fn add_daemon(
    State(s): State<AppState>,
    Json(body): Json<AddDaemonBody>,
) -> impl IntoResponse {
    // Try auth+status to verify the connection and get the fingerprint.
    let id = uuid::Uuid::new_v4().to_string();
    let req = MgmtRequest {
        id: id.clone(),
        method: methods::DAEMON_STATUS.into(),
        params: serde_json::Value::Null,
    };

    match s.noise.call(&body.addr, None, &body.token, req).await {
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
        Ok((_resp, fingerprint)) => {
            let entry = crate::registry::DaemonEntry {
                id: id.clone(),
                label: body.label,
                addr: body.addr,
                fingerprint: Some(fingerprint.clone()),
            };
            let mut reg = s.registry.write().await;
            if let Err(e) = reg.add(entry.clone()) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
            (
                StatusCode::CREATED,
                Json(json!({"id": id, "fingerprint": fingerprint})),
            )
                .into_response()
        }
    }
}

// ── Remove daemon ─────────────────────────────────────────────────────────────

pub async fn remove_daemon(
    State(s): State<AppState>,
    Path(daemon_id): Path<String>,
) -> impl IntoResponse {
    let mut reg = s.registry.write().await;
    match reg.remove(&daemon_id) {
        Ok(true) => (StatusCode::OK, Json(json!({"ok": true}))).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ── Status ────────────────────────────────────────────────────────────────────

pub async fn daemon_status(
    State(s): State<AppState>,
    Path(daemon_id): Path<String>,
) -> impl IntoResponse {
    let req = MgmtRequest {
        id: "1".into(),
        method: methods::DAEMON_STATUS.into(),
        params: serde_json::Value::Null,
    };
    try_call!(&s, &daemon_id, req)
}

// ── Container op ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ContainerOpBody {
    pub op: String,
}

pub async fn container_op(
    State(s): State<AppState>,
    Path(daemon_id): Path<String>,
    Json(body): Json<ContainerOpBody>,
) -> impl IntoResponse {
    let req = MgmtRequest {
        id: "1".into(),
        method: methods::CONTAINER_OP.into(),
        params: serde_json::to_value(ContainerOpParams { op: body.op }).unwrap(),
    };
    try_call!(&s, &daemon_id, req)
}

// ── Owner ─────────────────────────────────────────────────────────────────────

pub async fn owner_get(
    State(s): State<AppState>,
    Path(daemon_id): Path<String>,
) -> impl IntoResponse {
    let req = MgmtRequest {
        id: "1".into(),
        method: methods::OWNER_GET.into(),
        params: serde_json::Value::Null,
    };
    try_call!(&s, &daemon_id, req)
}

pub async fn owner_reset(
    State(s): State<AppState>,
    Path(daemon_id): Path<String>,
) -> impl IntoResponse {
    let req = MgmtRequest {
        id: "1".into(),
        method: methods::OWNER_RESET.into(),
        params: serde_json::Value::Null,
    };
    try_call!(&s, &daemon_id, req)
}

// ── Secrets ───────────────────────────────────────────────────────────────────

pub async fn list_secrets(
    State(s): State<AppState>,
    Path(daemon_id): Path<String>,
) -> impl IntoResponse {
    let req = MgmtRequest {
        id: "1".into(),
        method: methods::SECRET_LIST.into(),
        params: serde_json::Value::Null,
    };
    try_call!(&s, &daemon_id, req)
}

#[derive(Deserialize)]
pub struct SetSecretBody {
    pub key: String,
    pub value: String,
}

pub async fn set_secret(
    State(s): State<AppState>,
    Path(daemon_id): Path<String>,
    Json(body): Json<SetSecretBody>,
) -> impl IntoResponse {
    let req = MgmtRequest {
        id: "1".into(),
        method: methods::SECRET_SET.into(),
        params: serde_json::to_value(SecretSetParams {
            key: body.key,
            value: body.value,
        })
        .unwrap(),
    };
    try_call!(&s, &daemon_id, req)
}

pub async fn delete_secret(
    State(s): State<AppState>,
    Path((daemon_id, key)): Path<(String, String)>,
) -> impl IntoResponse {
    let req = MgmtRequest {
        id: "1".into(),
        method: methods::SECRET_DELETE.into(),
        params: serde_json::to_value(SecretDeleteParams { key }).unwrap(),
    };
    try_call!(&s, &daemon_id, req)
}

// ── Agent files ───────────────────────────────────────────────────────────────

pub async fn read_file(
    State(s): State<AppState>,
    Path((daemon_id, filename)): Path<(String, String)>,
) -> impl IntoResponse {
    let req = MgmtRequest {
        id: "1".into(),
        method: methods::AGENT_FILE_READ.into(),
        params: serde_json::to_value(AgentFileParams {
            filename,
            content: String::new(),
        })
        .unwrap(),
    };
    try_call!(&s, &daemon_id, req)
}

#[derive(Deserialize)]
pub struct WriteFileBody {
    pub content: String,
}

pub async fn write_file(
    State(s): State<AppState>,
    Path((daemon_id, filename)): Path<(String, String)>,
    Json(body): Json<WriteFileBody>,
) -> impl IntoResponse {
    let req = MgmtRequest {
        id: "1".into(),
        method: methods::AGENT_FILE_WRITE.into(),
        params: serde_json::to_value(AgentFileParams {
            filename,
            content: body.content,
        })
        .unwrap(),
    };
    try_call!(&s, &daemon_id, req)
}
