use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use bot_core::{AgentProfile, AgentRegistry};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::runtime_config::SetupState;
use crate::session::{Session, SessionRuntime};

#[derive(Clone)]
pub struct AdminState {
    pub agents_dir: PathBuf,
    pub sessions: Arc<Mutex<SessionRuntime>>,
    pub root_agent_id: String,
    pub setup_state: SetupState,
}

pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/api/agents", get(list_agents))
        .route(
            "/api/agents/{id}",
            get(get_agent).put(upsert_agent).delete(delete_agent),
        )
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}", get(get_session))
        .with_state(state)
}

async fn index(State(state): State<AdminState>) -> Html<String> {
    let setup_note = match &state.setup_state {
        SetupState::Initialized { .. } => "Setup status: initialized.".to_string(),
        SetupState::LegacyEnvCompatible { .. } => {
            "Setup status: legacy env mode (no runtime.yaml yet).".to_string()
        }
        SetupState::Uninitialized { .. } => {
            "Setup status: not initialized. Run `remi-cat setup`.".to_string()
        }
        SetupState::Invalid { error, .. } => {
            format!("Setup status: invalid runtime config. {error}")
        }
    };
    Html(format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>remi-cat admin</title>
<style>
body{{font-family:system-ui,sans-serif;max-width:880px;margin:40px auto;padding:0 20px;line-height:1.5}}
code{{background:#f2f2f2;padding:2px 5px;border-radius:4px}}
li{{margin:6px 0}}
</style>
<h1>remi-cat admin</h1>
<p>{}</p>
<ul>
  <li><code>GET /api/agents</code></li>
  <li><code>GET /api/agents/{{id}}</code></li>
  <li><code>PUT /api/agents/{{id}}</code> with <code>{{"markdown":"..."}}</code></li>
  <li><code>DELETE /api/agents/{{id}}</code></li>
  <li><code>GET /api/sessions</code></li>
  <li><code>GET /api/sessions/{{id}}</code></li>
</ul>
"#,
        setup_note
    ))
}

async fn health(State(state): State<AdminState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: if state.setup_state.is_initialized() {
            "ok"
        } else {
            "needs_setup"
        },
        root_agent_id: state.root_agent_id,
        initialized: state.setup_state.is_initialized(),
    })
}

async fn list_agents(
    State(state): State<AdminState>,
) -> Result<Json<Vec<AgentProfile>>, AdminError> {
    let registry = AgentRegistry::load(state.agents_dir)?;
    let mut agents: Vec<_> = registry.profiles().cloned().collect();
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(agents))
}

async fn get_agent(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<AgentProfile>, AdminError> {
    let registry = AgentRegistry::load(state.agents_dir)?;
    let profile = registry
        .get(&id)
        .cloned()
        .ok_or_else(|| AdminError::not_found(format!("agent `{id}` not found")))?;
    Ok(Json(profile))
}

async fn upsert_agent(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
    Json(req): Json<UpsertAgentRequest>,
) -> Result<Json<AgentProfile>, AdminError> {
    let profile = AgentProfile::from_markdown(&req.markdown)?;
    if profile.id != id {
        return Err(AdminError::bad_request(format!(
            "path id `{id}` does not match frontmatter id `{}`",
            profile.id
        )));
    }
    let file_name = format!("{id}.md");
    let mut registry = AgentRegistry::load(state.agents_dir)?;
    let profile = registry.upsert_markdown(&file_name, &req.markdown)?;
    Ok(Json(profile))
}

async fn delete_agent(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, AdminError> {
    let path = find_agent_file(&state.agents_dir, &id)?
        .ok_or_else(|| AdminError::not_found(format!("agent `{id}` not found")))?;
    std::fs::remove_file(&path)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_sessions(State(state): State<AdminState>) -> Json<Vec<Session>> {
    let mut sessions = state.sessions.lock().await.list();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Json(sessions)
}

async fn get_session(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Session>, AdminError> {
    let session = state
        .sessions
        .lock()
        .await
        .get(&id)
        .ok_or_else(|| AdminError::not_found(format!("session `{id}` not found")))?;
    Ok(Json(session))
}

fn find_agent_file(dir: &Path, id: &str) -> anyhow::Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        if let Ok(profile) = AgentProfile::from_markdown_file(&path) {
            if profile.id == id {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    root_agent_id: String,
    initialized: bool,
}

#[derive(Debug, Deserialize)]
struct UpsertAgentRequest {
    markdown: String,
}

#[derive(Debug)]
struct AdminError {
    status: StatusCode,
    message: String,
}

impl AdminError {
    fn not_found(message: String) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message,
        }
    }

    fn bad_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> axum::response::Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

impl From<anyhow::Error> for AdminError {
    fn from(err: anyhow::Error) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: err.to_string(),
        }
    }
}

impl From<std::io::Error> for AdminError {
    fn from(err: std::io::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
        }
    }
}
