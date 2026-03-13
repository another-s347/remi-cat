//! Docker container lifecycle management via the bollard API.

use anyhow::{Context, Result};
use bollard::container::{
    CreateContainerOptions, LogOutput, LogsOptions, RemoveContainerOptions,
    RestartContainerOptions, StartContainerOptions, StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::{ContainerStateStatusEnum, HostConfig};
use bollard::Docker;
use futures::StreamExt;
use mgmt_api::VolumeMount;
use tracing::{debug, info};

/// Wraps a [`Docker`] connection and provides high-level container operations
/// scoped to a single container name.
#[derive(Clone)]
pub struct DockerManager {
    docker: Docker,
    container_name: String,
}

impl DockerManager {
    /// Connect to Docker via the Unix socket at `/var/run/docker.sock`.
    pub fn new(container_name: impl Into<String>) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .context("failed to connect to Docker daemon — is /var/run/docker.sock accessible?")?;
        Ok(Self {
            docker,
            container_name: container_name.into(),
        })
    }

    pub fn container_name(&self) -> &str {
        &self.container_name
    }

    /// Restart the managed container.
    pub async fn restart(&self) -> Result<()> {
        info!(container = %self.container_name, "restarting container");
        self.docker
            .restart_container(
                &self.container_name,
                Some(RestartContainerOptions { t: 10 }),
            )
            .await
            .with_context(|| format!("restart_container({})", self.container_name))?;
        info!(container = %self.container_name, "container restarted");
        Ok(())
    }

    /// Stop the managed container.
    pub async fn stop(&self) -> Result<()> {
        info!(container = %self.container_name, "stopping container");
        self.docker
            .stop_container(&self.container_name, Some(StopContainerOptions { t: 10 }))
            .await
            .with_context(|| format!("stop_container({})", self.container_name))?;
        Ok(())
    }

    /// Start the managed container.
    pub async fn start(&self) -> Result<()> {
        info!(container = %self.container_name, "starting container");
        self.docker
            .start_container(&self.container_name, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("start_container({})", self.container_name))?;
        Ok(())
    }

    /// Return a short human-readable status string for the managed container.
    pub async fn status(&self) -> Result<String> {
        let info = self
            .docker
            .inspect_container(&self.container_name, None)
            .await
            .with_context(|| format!("inspect_container({})", self.container_name))?;

        let state = info.state.as_ref();
        let status = state
            .and_then(|s| s.status.as_ref())
            .map(|s| format!("{s:?}"))
            .unwrap_or_else(|| "unknown".to_string());

        let running = state.and_then(|s| s.running).unwrap_or(false);

        let started_at = state.and_then(|s| s.started_at.as_deref()).unwrap_or("-");

        Ok(format!(
            "Container `{}`: **{}** (running={}) since {}",
            self.container_name, status, running, started_at
        ))
    }

    /// Return whether the agent container is currently running.
    pub async fn is_running(&self) -> bool {
        match self
            .docker
            .inspect_container(&self.container_name, None)
            .await
        {
            Ok(info) => info
                .state
                .and_then(|s| s.status)
                .map(|s| s == ContainerStateStatusEnum::RUNNING)
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Pull the latest image for the managed container and return progress lines.
    /// The container's image name is derived from its current configuration.
    pub async fn pull_image(&self) -> Result<Vec<String>> {
        let info = self
            .docker
            .inspect_container(&self.container_name, None)
            .await
            .with_context(|| format!("inspect_container({})", self.container_name))?;

        let image = info
            .config
            .and_then(|c| c.image)
            .context("container has no image name")?;

        info!(container = %self.container_name, image = %image, "pulling image");

        let mut stream = self.docker.create_image(
            Some(CreateImageOptions {
                from_image: image.as_str(),
                ..Default::default()
            }),
            None,
            None,
        );

        let mut lines = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(info) => {
                    if let Some(status) = info.status {
                        debug!("pull: {status}");
                        lines.push(status);
                    }
                }
                Err(e) => return Err(e).context("image pull failed"),
            }
        }

        info!(container = %self.container_name, "image pull complete");
        Ok(lines)
    }

    /// Fetch the last `tail` lines of container logs and return them as a string.
    pub async fn logs(&self, tail: usize) -> Result<String> {
        let options = LogsOptions::<String> {
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            ..Default::default()
        };

        let mut stream = self.docker.logs(&self.container_name, Some(options));
        let mut out = String::new();

        while let Some(item) = stream.next().await {
            match item? {
                LogOutput::StdOut { message } | LogOutput::StdErr { message } => {
                    out.push_str(&String::from_utf8_lossy(&message));
                }
                _ => {}
            }
        }

        Ok(out)
    }

    /// Return the current bind mounts of the container as a list of
    /// `"host_path:container_path[:ro]"` strings.
    pub async fn list_mounts(&self) -> Result<Vec<VolumeMount>> {
        let info = self
            .docker
            .inspect_container(&self.container_name, None)
            .await
            .with_context(|| format!("inspect_container({})", self.container_name))?;

        let binds = info
            .host_config
            .as_ref()
            .and_then(|hc| hc.binds.as_ref())
            .cloned()
            .unwrap_or_default();

        let mounts = binds
            .into_iter()
            .filter_map(|b| parse_bind_string(&b))
            .collect();
        Ok(mounts)
    }

    /// Stop the container, remove it, recreate it with `extra_binds` added to
    /// its existing bind-mount configuration, then start it.
    ///
    /// `extra_binds` replaces the entire user-configured bind list — pass the
    /// full desired set (existing + new) from the caller.
    pub async fn recreate_with_mounts(&self, desired_mounts: &[VolumeMount]) -> Result<()> {
        info!(container = %self.container_name, "inspecting container before recreate");
        let info = self
            .docker
            .inspect_container(&self.container_name, None)
            .await
            .with_context(|| format!("inspect_container({})", self.container_name))?;

        // ── Collect existing config ───────────────────────────────────────
        let image = info
            .config
            .as_ref()
            .and_then(|c| c.image.clone())
            .context("container has no image")?;

        let env = info.config.as_ref().and_then(|c| c.env.clone());

        let cmd = info.config.as_ref().and_then(|c| c.cmd.clone());

        let entrypoint = info.config.as_ref().and_then(|c| c.entrypoint.clone());

        let labels = info.config.as_ref().and_then(|c| c.labels.clone());

        // Preserve existing binds that are NOT managed by us (named volumes,
        // etc.), and apply the full desired user-bind list on top.
        let existing_hc = info.host_config.as_ref();
        let existing_binds: Vec<String> = existing_hc
            .and_then(|hc| hc.binds.clone())
            .unwrap_or_default();

        // Keep binds that do NOT look like a host-path bind (i.e. named
        // volumes such as "remi-cat-data:/app/data") and add the new desired
        // host-path binds.
        let mut new_binds: Vec<String> = existing_binds
            .into_iter()
            .filter(|b| !b.starts_with('/'))
            .collect();
        for m in desired_mounts {
            let s = if m.read_only {
                format!("{}:{}:ro", m.host_path, m.container_path)
            } else {
                format!("{}:{}", m.host_path, m.container_path)
            };
            new_binds.push(s);
        }

        let restart_policy = existing_hc.and_then(|hc| hc.restart_policy.clone());
        let network_mode = existing_hc.and_then(|hc| hc.network_mode.clone());
        let extra_hosts = existing_hc.and_then(|hc| hc.extra_hosts.clone());

        // ── Stop & remove ────────────────────────────────────────────────
        info!(container = %self.container_name, "stopping container for recreate");
        let _ = self
            .docker
            .stop_container(&self.container_name, Some(StopContainerOptions { t: 10 }))
            .await; // ignore error if already stopped

        info!(container = %self.container_name, "removing container for recreate");
        self.docker
            .remove_container(
                &self.container_name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .with_context(|| format!("remove_container({})", self.container_name))?;

        // ── Recreate ────────────────────────────────────────────────────
        info!(container = %self.container_name, binds = ?new_binds, "creating container with updated mounts");

        let host_config = HostConfig {
            binds: if new_binds.is_empty() {
                None
            } else {
                Some(new_binds)
            },
            restart_policy,
            network_mode,
            extra_hosts,
            ..Default::default()
        };

        let config = bollard::container::Config {
            image: Some(image),
            env,
            cmd,
            entrypoint,
            labels,
            host_config: Some(host_config),
            ..Default::default()
        };

        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: self.container_name.as_str(),
                    platform: None,
                }),
                config,
            )
            .await
            .with_context(|| format!("create_container({})", self.container_name))?;

        // ── Start ────────────────────────────────────────────────────────
        self.docker
            .start_container(&self.container_name, None::<StartContainerOptions<String>>)
            .await
            .with_context(|| format!("start_container({})", self.container_name))?;

        info!(container = %self.container_name, "container recreated with updated mounts");
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse a Docker bind string such as `"/host/path:/container/path"` or
/// `"/host/path:/container/path:ro"` into a [`VolumeMount`].
/// Returns `None` for named-volume binds like `"volume-name:/app/data"`.
fn parse_bind_string(bind: &str) -> Option<VolumeMount> {
    let parts: Vec<&str> = bind.splitn(3, ':').collect();
    if parts.len() < 2 {
        return None;
    }
    // Only host-path binds start with '/'
    if !parts[0].starts_with('/') {
        return None;
    }
    let read_only = parts.get(2).map(|f| *f == "ro").unwrap_or(false);
    Some(VolumeMount {
        host_path: parts[0].to_string(),
        container_path: parts[1].to_string(),
        read_only,
    })
}
