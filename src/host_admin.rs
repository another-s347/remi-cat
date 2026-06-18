use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use bot_core::skill::store::SkillStore;
use bot_core::{
    remi_skill, trigger, AgentProfile, AgentRegistry, BuiltinSkillStore, FileSkillStore,
    ModelInputSnapshot, ToolApprovalDecision,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::runtime_config::SetupState;
use crate::secret_store::{apply_entries_to_env, redaction_entries, SecretStore};
use crate::session::{Session, SessionRuntime};
use crate::web_chat::{web_sdk_db_path, WebChatHandle, WEB_CHANNEL, WEB_USER_ID};
use crate::workspace_files::{
    default_file_search_limit, search_workspace_files, WorkspaceFileMatch,
};

const WEB_INDEX: &str = include_str!("../web-ui/dist/index.html");
const WEB_APP_JS: &str = include_str!("../web-ui/dist/app.js");
const WEB_APP_CSS: &str = include_str!("../web-ui/dist/app.css");

#[derive(Clone)]
pub struct AdminState {
    pub agents_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub workspace_root_label: String,
    pub secret_store: Arc<Mutex<SecretStore>>,
    pub sessions: Arc<Mutex<SessionRuntime>>,
    pub root_agent_id: String,
    pub setup_state: SetupState,
    pub web_chat: Option<WebChatHandle>,
}

pub fn router(state: AdminState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(web_app_js))
        .route("/app.css", get(web_app_css))
        .route("/health", get(health))
        .route("/api/agents", get(list_agents))
        .route(
            "/api/agents/{id}",
            get(get_agent).put(upsert_agent).delete(delete_agent),
        )
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}", get(get_session))
        .route("/api/v1/secrets", get(list_secrets).post(set_secret))
        .route("/api/v1/secrets/{key}", delete(delete_secret))
        .route("/api/v1/chat/commands", get(command_catalog))
        .route("/api/v1/chat/files", get(search_workspace_file_matches))
        .route(
            "/api/v1/chat/sessions",
            get(list_web_sessions).post(create_web_session),
        )
        .route(
            "/api/v1/chat/sessions/{id}",
            get(get_web_session)
                .patch(rename_web_session)
                .delete(delete_web_session),
        )
        .route("/api/v1/chat/sessions/{id}/fork", post(fork_web_session))
        .route(
            "/api/v1/chat/sessions/{id}/messages",
            get(web_session_messages),
        )
        .route(
            "/api/v1/chat/sessions/{id}/model-inputs",
            get(list_web_model_inputs),
        )
        .route(
            "/api/v1/chat/sessions/{id}/model-inputs/{run_id}",
            get(get_web_model_input),
        )
        .route("/api/v1/chat/sessions/{id}/todos", get(web_session_todos))
        .route(
            "/api/v1/chat/sessions/{id}/input-history",
            get(web_session_input_history).post(append_web_session_input_history),
        )
        .route(
            "/api/v1/chat/sessions/{id}/runs",
            post(start_web_run).delete(cancel_web_session_run),
        )
        .route(
            "/api/v1/chat/sessions/{id}/runs/active",
            get(active_web_session_run),
        )
        .route("/api/v1/chat/runs/{id}", delete(cancel_web_run))
        .route("/api/v1/chat/approvals/{id}", post(decide_web_approval))
        .route(
            "/api/v1/chat/assets/workspace/{*path}",
            get(workspace_image_asset),
        )
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(WEB_INDEX)
}

async fn web_app_js() -> Response {
    static_asset(WEB_APP_JS, "text/javascript; charset=utf-8")
}

async fn web_app_css() -> Response {
    static_asset(WEB_APP_CSS, "text/css; charset=utf-8")
}

fn static_asset(content: &'static str, content_type: &'static str) -> Response {
    let mut response = content.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
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

#[derive(Debug, Serialize)]
struct CommandCatalogEntry {
    value: String,
    label: String,
    description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    keywords: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    accepts_arguments: bool,
}

#[derive(Debug, Deserialize)]
struct FileSearchQuery {
    q: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct FileSearchResponse {
    items: Vec<WorkspaceFileMatch>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

async fn command_catalog(State(state): State<AdminState>) -> Json<Vec<CommandCatalogEntry>> {
    let mut entries = static_command_catalog();
    let store = BuiltinSkillStore::new(
        FileSkillStore::new(state.skills_dir),
        [
            trigger::builtin_trigger_skill(),
            remi_skill::builtin_remi_skill(),
        ],
    );
    let mut skills = store.featured_summaries();
    skills.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.source.cmp(&b.source)));
    entries.extend(skills.into_iter().map(|skill| CommandCatalogEntry {
        value: format!("/skill:{} ", skill.name),
        label: format!("Skill: {}", skill.name),
        description: skill.description,
        keywords: vec![
            "skill".to_string(),
            "技能".to_string(),
            skill.name,
            skill.source,
        ],
        accepts_arguments: true,
    }));
    Json(entries)
}

async fn search_workspace_file_matches(
    State(state): State<AdminState>,
    Query(query): Query<FileSearchQuery>,
) -> Result<Json<FileSearchResponse>, AdminError> {
    let raw_query = query.q.unwrap_or_default();
    let items = search_workspace_files(
        &state.workspace_dir,
        &state.workspace_root_label,
        &raw_query,
        default_file_search_limit(query.limit),
    )?;
    Ok(Json(FileSearchResponse { items }))
}

fn static_command_catalog() -> Vec<CommandCatalogEntry> {
    [
        (
            "/fork",
            "Fork Session",
            "复制当前会话并打开新的 Web session",
            false,
        ),
        ("/tools", "列出工具", "显示当前 Agent 可用的工具", false),
        (
            "/goal status",
            "查看 Goal",
            "查看当前会话的目标与监督状态",
            false,
        ),
        (
            "/goal set ",
            "设置 Goal",
            "设置目标；可选 --max-rounds N|unlimited",
            true,
        ),
        ("/goal clear", "清除 Goal", "清除当前会话的目标", false),
        (
            "/workflow status",
            "查看 Workflow",
            "查看当前 supervisor workflow 状态",
            false,
        ),
        (
            "/workflow start ",
            "启动 Workflow",
            "启动工作流；填写 ID 和可选参数",
            true,
        ),
        (
            "/workflow stop",
            "停止 Workflow",
            "停止当前 supervisor workflow",
            false,
        ),
        (
            "/compact",
            "压缩记忆",
            "立即将短期记忆压缩为中期摘要",
            false,
        ),
        (
            "/clear",
            "清空历史",
            "清空当前会话历史，保留 Todo 和 Trigger 状态",
            false,
        ),
        ("/doctor", "运行诊断", "显示 Remi 运行环境与配置诊断", false),
        ("/usage", "查询额度", "查询当前模型 API 账户额度", false),
        (
            "/model status",
            "模型状态",
            "显示当前 session 的模型配置状态",
            false,
        ),
        ("/model list", "模型列表", "列出可用模型 profile", false),
        (
            "/model use ",
            "切换模型",
            "切换当前 session 的模型 profile",
            true,
        ),
        (
            "/model reset",
            "重置模型",
            "清除当前 session 的模型 override",
            false,
        ),
        (
            "/permissions status",
            "权限模式",
            "查看当前 session 的工具审批策略",
            false,
        ),
        (
            "/permissions auto",
            "自动通过中低风险",
            "本 session 中 low/medium 自动通过，high 仍请求审批",
            false,
        ),
        (
            "/permissions ask",
            "恢复审批",
            "恢复默认：low 自动通过，medium/high 请求审批",
            false,
        ),
        (
            "/permissions allow",
            "信任本会话",
            "本 session 所有工具请求自动通过",
            false,
        ),
        ("/skill list", "Skill 列表", "列出可用 skills", false),
        (
            "/skill status",
            "Skill 状态",
            "显示当前 session 已读取的 skills",
            false,
        ),
    ]
    .into_iter()
    .map(
        |(value, label, description, accepts_arguments)| CommandCatalogEntry {
            value: value.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            keywords: value
                .trim_start_matches('/')
                .split_whitespace()
                .map(ToOwned::to_owned)
                .collect(),
            accepts_arguments,
        },
    )
    .collect()
}

#[derive(Debug, Serialize)]
struct SecretListResponse {
    backend: String,
    entries: Vec<SecretEntryResponse>,
}

#[derive(Debug, Serialize)]
struct SecretEntryResponse {
    key: String,
}

async fn list_secrets(
    State(state): State<AdminState>,
) -> Result<Json<SecretListResponse>, AdminError> {
    let store = state.secret_store.lock().await;
    let entries = store
        .keys()?
        .into_iter()
        .map(|key| SecretEntryResponse { key })
        .collect();
    Ok(Json(SecretListResponse {
        backend: store.backend_label(),
        entries,
    }))
}

#[derive(Debug, Deserialize)]
struct SetSecretRequest {
    key: String,
    value: String,
}

async fn set_secret(
    State(state): State<AdminState>,
    Json(request): Json<SetSecretRequest>,
) -> Result<Json<SecretListResponse>, AdminError> {
    {
        let store = state.secret_store.lock().await;
        store.set(request.key.trim(), &request.value)?;
    }
    sync_secrets_after_change(&state).await?;
    list_secrets(State(state)).await
}

async fn delete_secret(
    State(state): State<AdminState>,
    AxumPath(key): AxumPath<String>,
) -> Result<Json<SecretListResponse>, AdminError> {
    {
        let store = state.secret_store.lock().await;
        store.delete(key.trim())?;
    }
    unsafe {
        std::env::remove_var(key.trim());
    }
    sync_secrets_after_change(&state).await?;
    list_secrets(State(state)).await
}

async fn sync_secrets_after_change(state: &AdminState) -> Result<(), AdminError> {
    let entries = state.secret_store.lock().await.entries()?;
    apply_entries_to_env(&entries);
    if let Some(web_chat) = &state.web_chat {
        web_chat
            .update_secret_redactor(redaction_entries(&entries))
            .await?;
    }
    Ok(())
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

#[derive(Debug, Serialize)]
struct WebSessionResponse {
    #[serde(flatten)]
    session: Session,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_run: Option<crate::web_chat::ActiveWebRun>,
    sort_at: String,
}

async fn list_web_sessions(State(state): State<AdminState>) -> Json<Vec<WebSessionResponse>> {
    let active_runs = match web_handle(&state) {
        Ok(handle) => handle.active_runs().await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let active_by_session: std::collections::HashMap<_, _> = active_runs
        .into_iter()
        .map(|run| (run.session_id.clone(), run))
        .collect();
    let mut sessions: Vec<_> = state
        .sessions
        .lock()
        .await
        .list()
        .into_iter()
        .filter(|session| session.channel_binding.platform == WEB_CHANNEL)
        .map(|session| {
            let active_run = active_by_session.get(&session.id).cloned();
            let sort_at = active_run
                .as_ref()
                .map(|run| run.started_at.clone())
                .unwrap_or_else(|| session.updated_at.clone());
            WebSessionResponse {
                session,
                active_run,
                sort_at,
            }
        })
        .collect();
    sessions.sort_by(|a, b| b.sort_at.cmp(&a.sort_at));
    Json(sessions)
}

#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    title: Option<String>,
}

async fn create_web_session(
    State(state): State<AdminState>,
    payload: Option<Json<CreateSessionRequest>>,
) -> Result<(StatusCode, Json<Session>), AdminError> {
    let channel_id = uuid::Uuid::new_v4().to_string();
    let title = payload.and_then(|Json(payload)| payload.title);
    let session = state.sessions.lock().await.create_channel(
        WEB_CHANNEL,
        &channel_id,
        &state.root_agent_id,
        title,
    )?;
    Ok((StatusCode::CREATED, Json(session)))
}

async fn get_web_session(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Session>, AdminError> {
    Ok(Json(require_web_session(&state, &id).await?))
}

#[derive(Debug, Deserialize)]
struct RenameSessionRequest {
    title: String,
}

async fn rename_web_session(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<RenameSessionRequest>,
) -> Result<Json<Session>, AdminError> {
    require_web_session(&state, &id).await?;
    let session = state
        .sessions
        .lock()
        .await
        .rename(&id, &request.title)?
        .ok_or_else(|| AdminError::not_found(format!("session `{id}` not found")))?;
    Ok(Json(session))
}

async fn delete_web_session(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, AdminError> {
    require_web_session(&state, &id).await?;
    web_handle(&state)?.delete_thread(id.clone()).await?;
    delete_web_model_inputs_for_session(&state, &id);
    state.sessions.lock().await.delete(&id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct ForkSessionRequest {
    title: Option<String>,
}

async fn fork_web_session(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
    payload: Option<Json<ForkSessionRequest>>,
) -> Result<(StatusCode, Json<Session>), AdminError> {
    require_web_session(&state, &id).await?;
    if web_handle(&state)?.active_run(id.clone()).await?.is_some() {
        return Err(AdminError::conflict(
            "session has an active run; cancel or wait before forking".into(),
        ));
    }
    let title = payload.and_then(|Json(payload)| payload.title);
    let channel_id = uuid::Uuid::new_v4().to_string();
    let fork = state
        .sessions
        .lock()
        .await
        .fork_session(&id, WEB_CHANNEL, &channel_id, title)?
        .ok_or_else(|| AdminError::not_found(format!("session `{id}` not found")))?;
    let fork_result = web_handle(&state)?
        .fork_thread(id.clone(), fork.id.clone(), Some(WEB_USER_ID.to_string()))
        .await;
    if let Err(error) = fork_result {
        let _ = state.sessions.lock().await.delete(&fork.id);
        return Err(AdminError::from(error));
    }
    Ok((StatusCode::CREATED, Json(fork)))
}

async fn web_session_messages(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<bot_core::ThreadHistoryMessage>>, AdminError> {
    require_web_session(&state, &id).await?;
    Ok(Json(web_handle(&state)?.history(id).await?))
}

#[derive(Debug, Serialize)]
struct ModelInputSummary {
    run_id: String,
    created_at: String,
    model_profile_id: String,
    model: String,
    segment_count: usize,
    estimated_tokens: u32,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
    max_prompt_tokens: Option<u32>,
    context_tokens: Option<u32>,
}

async fn list_web_model_inputs(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<ModelInputSummary>>, AdminError> {
    require_web_session(&state, &id).await?;
    let sdk = web_model_input_sdk(&state)?;
    let items = sdk
        .list_chat_model_input_json(&id, Some(100))?
        .into_iter()
        .filter_map(|json| serde_json::from_str::<ModelInputSnapshot>(&json).ok())
        .map(model_input_summary)
        .collect();
    Ok(Json(items))
}

async fn get_web_model_input(
    State(state): State<AdminState>,
    AxumPath((id, run_id)): AxumPath<(String, String)>,
) -> Result<Json<ModelInputSnapshot>, AdminError> {
    require_web_session(&state, &id).await?;
    let sdk = web_model_input_sdk(&state)?;
    let json = sdk
        .get_chat_model_input_json(&id, &run_id)?
        .ok_or_else(|| AdminError::not_found("model input snapshot not found".into()))?;
    let snapshot = serde_json::from_str::<ModelInputSnapshot>(&json)
        .map_err(|err| AdminError::bad_request(format!("stored model input is invalid: {err}")))?;
    Ok(Json(snapshot))
}

fn model_input_summary(snapshot: ModelInputSnapshot) -> ModelInputSummary {
    ModelInputSummary {
        run_id: snapshot.run_id,
        created_at: snapshot.created_at,
        model_profile_id: snapshot.model_profile_id,
        model: snapshot.model,
        segment_count: snapshot.segments.len(),
        estimated_tokens: snapshot.totals.estimated_tokens,
        prompt_tokens: snapshot.totals.prompt_tokens,
        completion_tokens: snapshot.totals.completion_tokens,
        total_tokens: snapshot.totals.total_tokens,
        max_prompt_tokens: snapshot.totals.max_prompt_tokens,
        context_tokens: snapshot.totals.context_tokens,
    }
}

async fn web_session_todos(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<bot_core::todo::TodoItem>>, AdminError> {
    require_web_session(&state, &id).await?;
    Ok(Json(web_handle(&state)?.todos(id).await?))
}

#[derive(Debug, serde::Serialize)]
struct InputHistoryResponse {
    items: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct AppendInputHistoryRequest {
    text: String,
}

async fn web_session_input_history(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<InputHistoryResponse>, AdminError> {
    require_web_session(&state, &id).await?;
    let items = session_input_history(&state, &id).await;
    Ok(Json(InputHistoryResponse { items }))
}

async fn append_web_session_input_history(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<AppendInputHistoryRequest>,
) -> Result<Json<InputHistoryResponse>, AdminError> {
    require_web_session(&state, &id).await?;
    let mut sessions = state.sessions.lock().await;
    let existing = sessions
        .metadata_value(&id, crate::SESSION_INPUT_HISTORY_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
        .unwrap_or_default();
    let mut items = existing;
    items.push(request.text);
    let items = normalize_input_history(items);
    sessions.set_metadata_value(
        &id,
        crate::SESSION_INPUT_HISTORY_METADATA_KEY,
        serde_json::json!(items),
    )?;
    let items = sessions
        .metadata_value(&id, crate::SESSION_INPUT_HISTORY_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
        .unwrap_or_default();
    Ok(Json(InputHistoryResponse { items }))
}

async fn active_web_session_run(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Option<crate::web_chat::ActiveWebRun>>, AdminError> {
    require_web_session(&state, &id).await?;
    Ok(Json(web_handle(&state)?.active_run(id).await?))
}

#[derive(Debug, Deserialize)]
struct StartRunRequest {
    run_id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct ApprovalDecisionRequest {
    decision: ToolApprovalDecision,
}

async fn start_web_run(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<StartRunRequest>,
) -> Result<Response, AdminError> {
    require_web_session(&state, &id).await?;
    if request.text.trim().is_empty() {
        return Err(AdminError::bad_request("message may not be empty".into()));
    }
    uuid::Uuid::parse_str(&request.run_id)
        .map_err(|_| AdminError::bad_request("run_id must be a UUID".into()))?;
    if !crate::web_chat::is_web_fork_command(request.text.trim()) {
        state
            .sessions
            .lock()
            .await
            .set_title_if_empty(&id, &request.text)?;
    }
    let run = web_handle(&state)?
        .run(id, request.run_id, request.text)
        .await
        .map_err(|error| {
            if error.to_string().contains("already active") {
                AdminError::conflict(error.to_string())
            } else {
                AdminError::from(error)
            }
        })?;
    let stream = futures::stream::unfold(run.events, |mut events| async move {
        let event = events.recv().await?;
        let line = serde_json::to_string(&event).unwrap_or_else(|_| {
            "{\"version\":1,\"event\":\"error\",\"data\":{\"message\":\"serialization failed\"}}".into()
        });
        Some((
            Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!("{line}\n"))),
            events,
        ))
    });
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-transform"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

async fn cancel_web_session_run(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, AdminError> {
    require_web_session(&state, &id).await?;
    let _ = web_handle(&state)?.cancel_session(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn cancel_web_run(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, AdminError> {
    if web_handle(&state)?.cancel(id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AdminError::not_found("run not found".into()))
    }
}

async fn decide_web_approval(
    State(state): State<AdminState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<ApprovalDecisionRequest>,
) -> Result<Json<bot_core::ToolApprovalRequest>, AdminError> {
    let approval = web_handle(&state)?
        .decide_approval(id, request.decision)
        .await?
        .ok_or_else(|| AdminError::not_found("approval not found".into()))?;
    Ok(Json(approval))
}

async fn workspace_image_asset(
    State(state): State<AdminState>,
    AxumPath(path): AxumPath<String>,
) -> Result<Response, AdminError> {
    let content_type = image_content_type(&path)
        .ok_or_else(|| AdminError::not_found("workspace asset not found".into()))?;
    let full_path = resolve_workspace_asset_path(&state.workspace_dir, &path)?;
    let bytes = tokio::fs::read(&full_path).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            AdminError::not_found("workspace asset not found".into())
        } else {
            AdminError::from(err)
        }
    })?;
    let mut response = bytes.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn resolve_workspace_asset_path(root: &Path, path: &str) -> Result<PathBuf, AdminError> {
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            std::path::Component::Normal(part) => relative.push(part),
            _ => return Err(AdminError::not_found("workspace asset not found".into())),
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(AdminError::not_found("workspace asset not found".into()));
    }
    let root = std::fs::canonicalize(root)
        .map_err(|_| AdminError::not_found("workspace asset not found".into()))?;
    let full = std::fs::canonicalize(root.join(relative))
        .map_err(|_| AdminError::not_found("workspace asset not found".into()))?;
    if !full.starts_with(&root) {
        return Err(AdminError::not_found("workspace asset not found".into()));
    }
    Ok(full)
}

fn image_content_type(path: &str) -> Option<&'static str> {
    match Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("png") => Some("image/png"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

async fn require_web_session(state: &AdminState, id: &str) -> Result<Session, AdminError> {
    let session = state
        .sessions
        .lock()
        .await
        .get(id)
        .ok_or_else(|| AdminError::not_found(format!("session `{id}` not found")))?;
    if session.channel_binding.platform != WEB_CHANNEL {
        return Err(AdminError::not_found(format!("session `{id}` not found")));
    }
    Ok(session)
}

async fn session_input_history(state: &AdminState, id: &str) -> Vec<String> {
    state
        .sessions
        .lock()
        .await
        .metadata_value(id, crate::SESSION_INPUT_HISTORY_METADATA_KEY)
        .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
        .map(normalize_input_history)
        .unwrap_or_default()
}

fn normalize_input_history(items: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for item in items {
        let item = item.trim().to_string();
        if item.is_empty() || normalized.last() == Some(&item) {
            continue;
        }
        normalized.push(item);
    }
    if normalized.len() > 100 {
        normalized.split_off(normalized.len() - 100)
    } else {
        normalized
    }
}

fn web_handle(state: &AdminState) -> Result<&WebChatHandle, AdminError> {
    state.web_chat.as_ref().ok_or_else(|| AdminError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "web chat runtime is not available in admin-only mode".into(),
    })
}

fn web_model_input_sdk(state: &AdminState) -> Result<remi_client_sdk::TriggerSdk, AdminError> {
    let data_dir = admin_data_dir(state);
    let db_path = web_sdk_db_path(&data_dir, WEB_USER_ID);
    remi_client_sdk::TriggerSdk::initialize(&db_path).map_err(AdminError::from)
}

fn admin_data_dir(state: &AdminState) -> PathBuf {
    match &state.setup_state {
        SetupState::Initialized { config, .. } => PathBuf::from(&config.data_dir),
        SetupState::Invalid { config_path, .. } => config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| state.workspace_dir.clone()),
        SetupState::LegacyEnvCompatible { data_dir } | SetupState::Uninitialized { data_dir } => {
            data_dir.clone()
        }
    }
}

fn delete_web_model_inputs_for_session(state: &AdminState, session_id: &str) {
    match web_model_input_sdk(state) {
        Ok(sdk) => {
            if let Err(err) = sdk.delete_chat_model_inputs(session_id) {
                tracing::warn!(
                    session_id,
                    error = %err,
                    "failed to delete web model input snapshots"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                session_id,
                error = %err.message,
                "failed to initialize sdk while deleting web model input snapshots"
            );
        }
    }
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

    fn conflict(message: String) -> Self {
        Self {
            status: StatusCode::CONFLICT,
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
