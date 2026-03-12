//! Supervise a local `remi-cat-agent` child process.
//!
//! Used in `--local` mode: the daemon spawns the agent binary from the same
//! directory as itself (or `AGENT_BIN` env var), forwarding the gRPC address,
//! and automatically restarts it if it exits.

use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::process::{Child, Command};
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

pub struct LocalAgentSupervisor {
    agent_bin: PathBuf,
    daemon_addr: String,
}

impl LocalAgentSupervisor {
    /// Resolve the agent binary and store the gRPC address for child processes.
    pub fn new(daemon_addr: impl Into<String>) -> Result<Self> {
        let agent_bin = find_agent_bin()?;
        Ok(Self {
            agent_bin,
            daemon_addr: daemon_addr.into(),
        })
    }

    fn spawn_child(&self) -> Result<Child> {
        info!(bin = %self.agent_bin.display(), addr = %self.daemon_addr, "starting remi-cat");
        Command::new(&self.agent_bin)
            .env("DAEMON_ADDR", &self.daemon_addr)
            // Inherit stdout/stderr so agent logs appear in the same terminal.
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn {}", self.agent_bin.display()))
    }

    /// Run forever: spawn the agent, wait for it to exit, restart after 2 s.
    pub async fn supervise(self) {
        loop {
            match self.spawn_child() {
                Err(e) => {
                    error!("cannot start remi-cat: {e:#}");
                }
                Ok(mut child) => match child.wait().await {
                    Ok(status) if status.success() => {
                        info!("remi-cat exited cleanly");
                    }
                    Ok(status) => {
                        warn!("remi-cat exited with {status}");
                    }
                    Err(e) => {
                        error!("waiting for remi-cat failed: {e:#}");
                    }
                },
            }
            info!("restarting remi-cat in 2 s…");
            sleep(Duration::from_secs(2)).await;
        }
    }
}

fn find_agent_bin() -> Result<PathBuf> {
    // 1. Explicit override via env var.
    if let Ok(p) = std::env::var("AGENT_BIN") {
        return Ok(PathBuf::from(p));
    }

    // 2. Same directory as the running daemon binary.
    let exe = std::env::current_exe().context("cannot determine current executable path")?;
    let dir = exe.parent().unwrap_or_else(|| std::path::Path::new("."));
    let candidate = dir.join("remi-cat-agent");
    if candidate.exists() {
        return Ok(candidate);
    }

    anyhow::bail!(
        "cannot find remi-cat-agent binary (tried {candidate:?}); \
         set AGENT_BIN env var to specify the path"
    )
}
