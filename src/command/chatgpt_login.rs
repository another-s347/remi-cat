use anyhow::{bail, ensure, Context, Result};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use bot_core::chatgpt::LoginCallback;
use clap::ValueEnum;
use serde_json::{json, Value};
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    net::TcpListener,
    process::{Child, ChildStdout, Command},
    sync::{oneshot, Mutex},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub(crate) enum CallbackHost {
    /// Receive in Windows for a local WSL session, otherwise in this process.
    #[default]
    Auto,
    /// Receive on this machine; use SSH port forwarding for a remote browser.
    Local,
    /// Receive in Windows and transport the callback over the WSL process pipe.
    Windows,
}

pub(crate) enum CallbackServer {
    Local(Option<TcpListener>),
    Windows {
        child: Child,
        lines: Lines<BufReader<ChildStdout>>,
        port: u16,
    },
}

fn in_wsl() -> bool {
    cfg!(target_os = "linux")
        && (std::env::var_os("WSL_INTEROP").is_some()
            || std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .is_ok_and(|s| s.to_ascii_lowercase().contains("microsoft")))
}
fn remote_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}
fn use_windows(host: CallbackHost, wsl: bool, remote: bool) -> bool {
    host == CallbackHost::Windows || (host == CallbackHost::Auto && wsl && !remote)
}
fn powershell() -> PathBuf {
    let mounted = PathBuf::from("/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe");
    if mounted.is_file() {
        mounted
    } else {
        "powershell.exe".into()
    }
}
fn encoded_powershell(script: &str) -> Command {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut command = Command::new(powershell());
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &STANDARD.encode(bytes),
        ])
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

impl CallbackServer {
    pub(crate) async fn bind(host: CallbackHost, port: u16) -> Result<Self> {
        if !use_windows(host, in_wsl(), remote_session()) {
            return Ok(Self::Local(Some(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
                .context("Cannot start the loopback callback. Choose another --port or omit it to use a free port")?)));
        }
        ensure!(
            in_wsl(),
            "--callback-host windows requires WSL with Windows interoperability"
        );
        let script = include_str!("chatgpt_login/windows_callback.ps1")
            .replace("__PORT__", &port.to_string());
        let mut child = encoded_powershell(&script).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()
            .context("Cannot start the Windows callback listener. Enable WSL interoperability, or use --callback-host local with SSH port forwarding")?;
        let mut lines = BufReader::new(
            child
                .stdout
                .take()
                .context("Windows callback pipe unavailable")?,
        )
        .lines();
        let ready = tokio::time::timeout(Duration::from_secs(15), lines.next_line()).await;
        let port = match ready {
            Ok(Ok(Some(line))) => serde_json::from_str::<Value>(&line)
                .ok()
                .and_then(|v| v["port"].as_u64())
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p != 0),
            _ => None,
        };
        let Some(port) = port else {
            let _ = child.kill().await;
            bail!("Windows could not start a loopback listener. Omit --port to avoid reserved ports. If Windows interoperability is disabled, use --callback-host local with SSH port forwarding. No authorization was started.");
        };
        Ok(Self::Windows { child, lines, port })
    }
    pub(crate) fn port(&self) -> Result<u16> {
        match self {
            Self::Local(listener) => Ok(listener
                .as_ref()
                .context("Callback listener already started")?
                .local_addr()?
                .port()),
            Self::Windows { port, .. } => Ok(*port),
        }
    }
    pub(crate) fn is_windows(&self) -> bool {
        matches!(self, Self::Windows { .. })
    }
    pub(crate) async fn receive(&mut self, state: &str) -> Result<LoginCallback> {
        match self {
            Self::Windows { child, lines, .. } => {
                let config = format!("{}\n", json!({"state":state}));
                child
                    .stdin
                    .as_mut()
                    .context("Windows callback input unavailable")?
                    .write_all(config.as_bytes())
                    .await?;
                let line = lines
                    .next_line()
                    .await
                    .context("Windows callback pipe failed")?
                    .context("Windows callback listener closed; run login again")?;
                // Never include pipe contents in diagnostic errors.
                let value: Value = serde_json::from_str(&line)
                    .map_err(|_| anyhow::anyhow!("Invalid Windows callback reply"))?;
                let target = value["callback"]
                    .as_str()
                    .context("Windows callback listener failed; run login again")?;
                parse_target(target, state)
            }
            Self::Local(listener) => {
                let (tx, rx) = oneshot::channel();
                let app = Router::new()
                    .route("/auth/callback", get(callback))
                    .with_state(CallbackState {
                        state: state.into(),
                        sender: Arc::new(Mutex::new(Some(tx))),
                    });
                let listener = listener
                    .take()
                    .context("Callback listener already started")?;
                let (stop_tx, stop_rx) = oneshot::channel::<()>();
                let server = async {
                    axum::serve(listener, app)
                        .with_graceful_shutdown(async {
                            let _ = stop_rx.await;
                        })
                        .await
                };
                tokio::pin!(server);
                let received = tokio::select! {
                    result = rx => result.context("Callback listener closed; run login again"),
                    result = &mut server => { result.context("Callback listener failed")?; bail!("Callback listener stopped before authorization"); }
                };
                let _ = stop_tx.send(());
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
                received
            }
        }
    }
    pub(crate) async fn stop(&mut self) {
        if let Self::Windows { child, .. } = self {
            let _ = child.kill().await;
        }
    }
}

fn parse_target(target: &str, expected: &str) -> Result<LoginCallback> {
    let uri: http::Uri = target
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid callback path"))?;
    ensure!(
        uri.scheme().is_none() && uri.authority().is_none() && uri.path() == "/auth/callback",
        "Invalid callback path"
    );
    let Query(query) = Query::<LoginCallback>::try_from_uri(&uri)
        .map_err(|_| anyhow::anyhow!("Invalid callback parameters"))?;
    validate_callback(&query, expected)?;
    Ok(query)
}
fn validate_callback(query: &LoginCallback, expected: &str) -> Result<()> {
    ensure!(
        query.state.as_deref() == Some(expected),
        "Invalid authorization state; use the current login attempt"
    );
    ensure!(
        query.code.as_deref().is_some_and(|s| !s.is_empty())
            || query.error.as_deref().is_some_and(|s| !s.is_empty()),
        "Callback has no authorization result"
    );
    Ok(())
}
#[derive(Clone)]
struct CallbackState {
    state: String,
    sender: Arc<Mutex<Option<oneshot::Sender<LoginCallback>>>>,
}
async fn callback(
    State(state): State<CallbackState>,
    Query(query): Query<LoginCallback>,
) -> (
    StatusCode,
    [(axum::http::header::HeaderName, &'static str); 2],
    &'static str,
) {
    let headers = [
        (axum::http::header::CACHE_CONTROL, "no-store"),
        (axum::http::header::REFERRER_POLICY, "no-referrer"),
    ];
    if validate_callback(&query, &state.state).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            headers,
            "Invalid authorization callback. Use the current login attempt.",
        );
    }
    if let Some(sender) = state.sender.lock().await.take() {
        let _ = sender.send(query);
        (
            StatusCode::OK,
            headers,
            "Authorization received. Return to remi-cat to see the result. You can close this tab.",
        )
    } else {
        (
            StatusCode::CONFLICT,
            headers,
            "Authorization was already received.",
        )
    }
}

pub(crate) async fn open_browser(url: &str, windows: bool) -> bool {
    if remote_session() {
        return false;
    }
    let mut command = if windows || in_wsl() {
        let payload = STANDARD.encode(url.as_bytes());
        encoded_powershell(&format!("$ErrorActionPreference='Stop'; $url=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{payload}')); Start-Process -FilePath $url"))
    } else {
        #[cfg(windows)]
        let mut command = {
            let mut c = Command::new("rundll32.exe");
            c.arg("url.dll,FileProtocolHandler");
            c
        };
        #[cfg(target_os = "macos")]
        let mut command = Command::new("open");
        #[cfg(all(unix, not(target_os = "macos")))]
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    matches!(tokio::time::timeout(Duration::from_secs(10), command.status()).await, Ok(Ok(status)) if status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn local_callback_completes_http_response_and_releases_port() {
        let mut server = CallbackServer::bind(CallbackHost::Local, 0).await.unwrap();
        let port = server.port().unwrap();
        let request = async {
            let response = reqwest::get(format!(
                "http://127.0.0.1:{port}/auth/callback?state=expected&error=access_denied"
            ))
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(response
                .text()
                .await
                .unwrap()
                .contains("Return to remi-cat"));
        };
        let (callback, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server.receive("expected"), request)
        })
        .await
        .unwrap();
        assert_eq!(callback.unwrap().error.as_deref(), Some("access_denied"));
        drop(server);
        assert!(TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_ok());
    }
    #[tokio::test]
    #[ignore = "Requires WSL Windows interoperability; uses synthetic callbacks, no account"]
    async fn windows_loopback_real_ipc_rejects_wrong_state_and_cleans_up() {
        let mut server = CallbackServer::bind(CallbackHost::Windows, 0)
            .await
            .unwrap();
        let port = server.port().unwrap();
        let script = format!(
            r#"
$ErrorActionPreference='Stop'
try {{
    Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:{port}/auth/callback?state=wrong&code=test-only' | Out-Null
    exit 1
}} catch {{
    if ([int]$_.Exception.Response.StatusCode -ne 400) {{ exit 2 }}
}}
$response=Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:{port}/auth/callback?state=expected&code=test-only&client_id=test-client'
if ($response.StatusCode -ne 200) {{ exit 3 }}
"#
        );
        let mut client = encoded_powershell(&script);
        client.stdin(Stdio::null()).stdout(Stdio::null());
        let result = tokio::time::timeout(Duration::from_secs(25), async {
            tokio::join!(server.receive("expected"), client.status())
        })
        .await;
        server.stop().await;
        let (callback, status) = result.unwrap();
        assert!(
            status.unwrap().success(),
            "Windows HTTP callback probe failed"
        );
        let callback = callback.unwrap();
        assert_eq!(callback.code.as_deref(), Some("test-only"));
        assert_eq!(callback.client_id.as_deref(), Some("test-client"));
        let mut rebound = CallbackServer::bind(CallbackHost::Windows, port)
            .await
            .unwrap();
        rebound.stop().await;
        if let CallbackServer::Windows { child, .. } = &mut rebound {
            assert!(child.try_wait().unwrap().is_some());
        }
    }
    #[test]
    fn callback_host_respects_remote_and_explicit_selection() {
        assert!(use_windows(CallbackHost::Auto, true, false));
        assert!(!use_windows(CallbackHost::Auto, true, true));
        assert!(!use_windows(CallbackHost::Auto, false, false));
        assert!(!use_windows(CallbackHost::Local, true, false));
        assert!(use_windows(CallbackHost::Windows, true, true));
    }
    #[test]
    fn callback_validates_path_state_and_result_without_echoing_secrets() {
        assert!(parse_target(
            "/auth/callback?state=expected&code=secret&client_id=issued",
            "expected"
        )
        .is_ok());
        for target in [
            "/auth/callback?state=old&code=secret",
            "/auth/callback?state=expected",
            "/wrong?state=expected&code=secret",
            "http://evil/auth/callback?state=expected&code=secret",
            "/auth/callback?state=expected&state=old&code=secret",
        ] {
            let error = parse_target(target, "expected").err().unwrap().to_string();
            assert!(!error.contains("secret"));
        }
        assert!(parse_target(
            "/auth/callback?state=expected&error=access_denied",
            "expected"
        )
        .is_ok());
    }
    #[tokio::test]
    async fn invalid_callback_does_not_consume_login() {
        let (tx, mut rx) = oneshot::channel();
        let state = CallbackState {
            state: "expected".into(),
            sender: Arc::new(Mutex::new(Some(tx))),
        };
        let query = |state: &str| {
            Query(LoginCallback {
                state: Some(state.into()),
                code: Some("secret".into()),
                client_id: None,
                error: None,
            })
        };
        assert_eq!(
            callback(State(state.clone()), query("wrong")).await.0,
            StatusCode::BAD_REQUEST
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            callback(State(state), query("expected")).await.0,
            StatusCode::OK
        );
        assert_eq!(rx.await.unwrap().code.as_deref(), Some("secret"));
    }
}
