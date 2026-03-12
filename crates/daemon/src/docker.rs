//! Docker container lifecycle management via the bollard API.

use anyhow::{Context, Result};
use bollard::container::{
    LogOutput, LogsOptions, RestartContainerOptions, StartContainerOptions,
    StopContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::ContainerStateStatusEnum;
use bollard::Docker;
use futures::StreamExt;
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
            .stop_container(
                &self.container_name,
                Some(StopContainerOptions { t: 10 }),
            )
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

        let running = state
            .and_then(|s| s.running)
            .unwrap_or(false);

        let started_at = state
            .and_then(|s| s.started_at.as_deref())
            .unwrap_or("-");

        Ok(format!(
            "Container `{}`: **{}** (running={}) since {}",
            self.container_name, status, running, started_at
        ))
    }

    /// Return whether the agent container is currently running.
    pub async fn is_running(&self) -> bool {
        match self.docker.inspect_container(&self.container_name, None).await {
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
}
