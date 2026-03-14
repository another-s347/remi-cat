//! `remi-admin` — Local management UI for remi-daemon instances.
//!
//! Starts a local web server on `localhost:8770` (configurable via
//! `REMI_ADMIN_PORT`), and proxies management
//! requests to one or more remote daemons over Noise_XX-encrypted WebSocket.
//!
//! # Usage
//!
//! ```
//! remi-admin [--port PORT] [--config-dir PATH]
//! ```
//!
//! # Environment variables
//!
//! | Variable            | Default                      | Description        |
//! |---------------------|------------------------------|--------------------|
//! | `REMI_ADMIN_PORT`   | `8770`                       | Local HTTP port    |
//! | `REMI_ADMIN_CONFIG` | `~/.config/remi-admin`       | Config directory   |

mod api;
mod noise_client;
mod registry;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    response::Html,
    routing::{delete, get, post},
    Router,
};
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;
use tracing::info;

use api::AppState;
use noise_client::NoiseClient;
use registry::Registry;

const UI_HTML: &str = include_str!("static/index.html");

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "remi_admin=info".into()),
        )
        .init();

    // ── Config dir ────────────────────────────────────────────────────────
    let config_dir = std::env::var("REMI_ADMIN_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from(".config"))
                .join("remi-admin")
        });

    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("creating config dir {config_dir:?}"))?;

    // ── Port ──────────────────────────────────────────────────────────────
    let port: u16 = std::env::var("REMI_ADMIN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8770);

    // CLI override: --port N
    let args: Vec<String> = std::env::args().collect();
    let port = args
        .windows(2)
        .find(|w| w[0] == "--port")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(port);

    // ── Noise identity ────────────────────────────────────────────────────
    let noise = Arc::new(NoiseClient::load_or_create(&config_dir)?);

    // ── Registry ──────────────────────────────────────────────────────────
    let registry = Arc::new(RwLock::new(Registry::load(&config_dir)?));

    let state = AppState { registry, noise };

    // ── Router ────────────────────────────────────────────────────────────
    let app = Router::new()
        // UI
        .route("/", get(serve_ui))
        // Daemon CRUD
        .route("/api/daemons", get(api::list_daemons).post(api::add_daemon))
        .route("/api/daemons/:id", delete(api::remove_daemon))
        // Status
        .route("/api/daemons/:id/status", get(api::daemon_status))
        // Container ops
        .route("/api/daemons/:id/container", post(api::container_op))
        // Owner
        .route(
            "/api/daemons/:id/owner",
            get(api::owner_get).delete(api::owner_reset),
        )
        // Secrets
        .route(
            "/api/daemons/:id/secrets",
            get(api::list_secrets).post(api::set_secret),
        )
        .route("/api/daemons/:id/secrets/:key", delete(api::delete_secret))
        // Agent files
        .route(
            "/api/daemons/:id/files/:name",
            get(api::read_file).put(api::write_file),
        )
        // Volume mounts
        .route(
            "/api/daemons/:id/volumes",
            get(api::list_volumes).post(api::add_volume),
        )
        .route("/api/daemons/:id/volumes/remove", post(api::remove_volume))
        // Users
        .route("/api/daemons/:id/users", get(api::list_users))
        .route("/api/daemons/:id/users/banned", get(api::list_banned_users))
        .route("/api/daemons/:id/users/link", post(api::link_users))
        .route("/api/daemons/:id/users/unlink", post(api::unlink_user))
        .route("/api/daemons/:id/users/:uuid", delete(api::delete_user))
        .route(
            "/api/daemons/:id/users/:uuid/ban",
            post(api::ban_user).delete(api::unban_user),
        )
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("127.0.0.1:{port}");
    let url = format!("http://localhost:{port}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;

    info!("remi-admin listening on {url}");

    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_ui() -> Html<&'static str> {
    Html(UI_HTML)
}
