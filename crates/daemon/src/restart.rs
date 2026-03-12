//! Daemon self-restart with "new process must confirm startup before old one exits".
//!
//! # Protocol
//!
//! 1. Old process (parent):
//!    - Creates a named FIFO at `/tmp/remi-daemon-ready-<pid>.fifo`.
//!    - Spawns a new child process via `std::process::Command`, passing the
//!      FIFO path in the `REMI_DAEMON_READY_FIFO` environment variable.
//!    - Waits up to 30 s for the child to write `"ok"` to the FIFO.
//!    - If `"ok"` arrives: calls `std::process::exit(0)`.
//!    - If the child crashes or times out: removes the FIFO, returns an error
//!      and continues serving normally.
//!
//! 2. New process (child):
//!    - Detects `REMI_DAEMON_READY_FIFO` at startup.
//!    - After successfully binding the gRPC listener, calls
//!      [`signal_ready`] which writes `"ok"` to the FIFO.
//!
//! # Self-update
//!
//! When a URL is provided to [`RestartHandle::spawn_restart`], the binary is
//! downloaded to a temporary path then atomically renamed over the current
//! executable before the child is spawned.  If the rename or download fails
//! the restart is aborted and an error is returned.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{error, info, warn};

/// Name of the environment variable used to pass the ready-FIFO path.
pub const READY_FIFO_ENV: &str = "REMI_DAEMON_READY_FIFO";

// ── RestartHandle ─────────────────────────────────────────────────────────────

/// A handle that can be cloned and shared; used to trigger daemon restarts.
#[derive(Clone)]
pub struct RestartHandle;

impl RestartHandle {
    pub fn new() -> Self {
        RestartHandle
    }

    /// Spawn a new daemon process and wait (asynchronously) for it to confirm
    /// successful startup.
    ///
    /// * `update_url`: if `Some`, download a new binary from this URL and
    ///   atomically replace the current executable before spawning.
    ///
    /// Returns `Ok(())` once the child has confirmed startup; the *caller*
    /// should return and allow the Tokio runtime to wind down naturally
    /// (or the daemon's main loop to call `std::process::exit`).
    pub async fn spawn_restart(&self, update_url: Option<String>) -> Result<()> {
        // ── Step 1: optional self-update ──────────────────────────────────
        if let Some(url) = update_url {
            download_and_replace(&url)
                .await
                .context("self-update download failed — restart aborted")?;
        }

        // ── Step 2: create the ready FIFO ─────────────────────────────────
        let pid = std::process::id();
        let fifo_path = format!("/tmp/remi-daemon-ready-{pid}.fifo");

        // Remove any stale FIFO from a previous attempt.
        let _ = std::fs::remove_file(&fifo_path);

        nix::unistd::mkfifo(
            fifo_path.as_str(),
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .with_context(|| format!("mkfifo({fifo_path})"))?;

        // ── Step 3: spawn child ───────────────────────────────────────────
        let exe = std::fs::read_link("/proc/self/exe")
            .unwrap_or_else(|_| PathBuf::from(std::env::current_exe().unwrap()));

        let child = std::process::Command::new(&exe)
            .env(READY_FIFO_ENV, &fifo_path)
            .args(std::env::args_os().skip(1))
            .spawn()
            .with_context(|| format!("spawn({exe:?})"))?;

        info!(pid = child.id(), exe = ?exe, "spawned new daemon process");

        // ── Step 4: wait for child to signal readiness (30 s timeout) ─────
        let fifo_path_clone = fifo_path.clone();
        let ready = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::task::spawn_blocking(move || -> Result<bool> {
                use std::io::Read;
                // Opening a FIFO for reading blocks until the write end is opened.
                let mut f = std::fs::File::open(&fifo_path_clone)
                    .with_context(|| format!("open FIFO {fifo_path_clone}"))?;
                let mut buf = [0u8; 2];
                f.read_exact(&mut buf)?;
                Ok(&buf == b"ok")
            }),
        )
        .await;

        // ── Cleanup FIFO ──────────────────────────────────────────────────
        let _ = std::fs::remove_file(&fifo_path);

        match ready {
            Ok(Ok(Ok(true))) => {
                info!("new daemon process confirmed startup — exiting");
                // Give a moment for the reply message to be sent before exiting.
                tokio::time::sleep(Duration::from_millis(500)).await;
                std::process::exit(0);
            }
            Ok(Ok(Ok(false))) => {
                error!("new daemon process sent unexpected signal — restart aborted");
                Err(anyhow::anyhow!("unexpected startup signal from child process"))
            }
            Ok(Ok(Err(e))) => {
                error!("FIFO read error: {e:#}");
                Err(e.context("failed to read startup signal from child"))
            }
            Ok(Err(join_err)) => {
                error!("spawn_blocking panicked: {join_err}");
                Err(anyhow::anyhow!("internal error during restart wait"))
            }
            Err(_timeout) => {
                warn!("new daemon process did not confirm startup within 30 s — restart aborted");
                Err(anyhow::anyhow!(
                    "new daemon process timed out — original process continues serving"
                ))
            }
        }
    }
}

// ── Child-side: signal readiness ──────────────────────────────────────────────

/// Called by the new daemon process after the gRPC listener is successfully
/// bound.  Writes `"ok"` to the FIFO so the parent process can exit.
pub fn signal_ready() {
    let Some(fifo_path) = std::env::var(READY_FIFO_ENV).ok() else {
        return; // Not a restart child — nothing to signal.
    };

    use std::io::Write;
    match std::fs::OpenOptions::new()
        .write(true)
        .open(&fifo_path)
        .and_then(|mut f| f.write_all(b"ok"))
    {
        Ok(()) => info!("signalled readiness to parent via {fifo_path}"),
        Err(e) => warn!("failed to signal readiness to parent: {e}"),
    }
}

// ── Binary download ───────────────────────────────────────────────────────────

/// Download the binary at `url`, write it to a temporary file, set executable
/// permissions, then atomically rename it over the current executable.
async fn download_and_replace(url: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;

    info!(url, "downloading new daemon binary");

    let response = reqwest::get(url)
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("server error downloading {url}"))?;

    let tmp_path = format!("/tmp/remi-daemon-new-{}", std::process::id());

    {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .await
            .with_context(|| format!("create temp file {tmp_path}"))?;

        let mut byte_stream = response.bytes_stream();
        use futures::StreamExt;
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.context("download stream error")?;
            file.write_all(&chunk).await.context("write temp file")?;
        }
        file.flush().await.context("flush temp file")?;
    }

    // Make executable.
    tokio::fs::set_permissions(
        &tmp_path,
        std::fs::Permissions::from_mode(0o755),
    )
    .await
    .context("chmod temp binary")?;

    // Atomic rename over the current exe.
    let current_exe = std::fs::read_link("/proc/self/exe")
        .unwrap_or_else(|_| std::env::current_exe().unwrap());

    tokio::fs::rename(&tmp_path, &current_exe)
        .await
        .with_context(|| {
            format!("rename {tmp_path} → {current_exe:?} (ensure you have write permission)")
        })?;

    info!(path = ?current_exe, "daemon binary replaced");
    Ok(())
}
