use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::timeout;
use tonic::Request;
use tonic::transport::Channel;

// Include generated proto code (same package as trigger_client)
pub mod proto {
    tonic::include_proto!("public_api.v1");
}

use proto::{
    ApiKeyInfo, ApplicationInfo, CreateApiKeyRequest, CreateApplicationRequest,
    DeleteApplicationRequest, ListApiKeysRequest, ListApplicationsRequest, RevokeApiKeyRequest,
    public_service_client::PublicServiceClient,
};

/// Client for managing applications and their API keys on the server.
pub struct AppKeysClient {
    client: PublicServiceClient<Channel>,
    user_access_token: String,
    request_timeout: Duration,
}

impl AppKeysClient {
    /// Create an app-keys client that reuses the shared transport configured for auth/telemetry.
    pub async fn new_with_shared_transport(user_access_token: impl Into<String>) -> Result<Self> {
        let transport =
            crate::transport::get_shared_transport().map_err(|err| anyhow::anyhow!(err))?;
        let request_timeout = transport.request_timeout();
        let channel = transport
            .get_channel()
            .await
            .map_err(|err| anyhow::anyhow!(err))?;

        let client = PublicServiceClient::new(channel);

        Ok(Self {
            client,
            user_access_token: user_access_token.into(),
            request_timeout,
        })
    }

    // ─── Applications ─────────────────────────────────────────────────────────

    /// Create a new named application for the authenticated user.
    pub async fn create_application(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
    ) -> Result<ApplicationInfo> {
        let request = Request::new(CreateApplicationRequest {
            name: name.into(),
            description: description.into(),
        });
        let request = self.add_auth_header(request).await?;
        let response = timeout(
            self.request_timeout,
            self.client.create_application(request),
        )
        .await
        .context("create_application timed out")??
        .into_inner();
        response
            .application
            .context("Server returned no application")
    }

    /// List all applications owned by the authenticated user.
    pub async fn list_applications(&mut self) -> Result<Vec<ApplicationInfo>> {
        let request = Request::new(ListApplicationsRequest {});
        let request = self.add_auth_header(request).await?;
        let response = timeout(self.request_timeout, self.client.list_applications(request))
            .await
            .context("list_applications timed out")??
            .into_inner();
        Ok(response.applications)
    }

    /// Delete an application and all its API keys.
    pub async fn delete_application(&mut self, app_id: impl Into<String>) -> Result<()> {
        let request = Request::new(DeleteApplicationRequest {
            app_id: app_id.into(),
        });
        let request = self.add_auth_header(request).await?;
        timeout(
            self.request_timeout,
            self.client.delete_application(request),
        )
        .await
        .context("delete_application timed out")??;
        Ok(())
    }

    // ─── API Keys ──────────────────────────────────────────────────────────────

    /// Create a new API key for the given application.
    ///
    /// Returns `(key_info, plaintext_key)`. The `plaintext_key` is shown exactly once
    /// and is never stored on the server; the caller must present it to the user immediately.
    pub async fn create_api_key(
        &mut self,
        app_id: impl Into<String>,
    ) -> Result<(ApiKeyInfo, String)> {
        let request = Request::new(CreateApiKeyRequest {
            app_id: app_id.into(),
        });
        let request = self.add_auth_header(request).await?;
        let response = timeout(self.request_timeout, self.client.create_api_key(request))
            .await
            .context("create_api_key timed out")??
            .into_inner();
        let key_info = response.key.context("Server returned no key info")?;
        Ok((key_info, response.plaintext_key))
    }

    /// List all API keys for the given application (plaintext never returned).
    pub async fn list_api_keys(&mut self, app_id: impl Into<String>) -> Result<Vec<ApiKeyInfo>> {
        let request = Request::new(ListApiKeysRequest {
            app_id: app_id.into(),
        });
        let request = self.add_auth_header(request).await?;
        let response = timeout(self.request_timeout, self.client.list_api_keys(request))
            .await
            .context("list_api_keys timed out")??
            .into_inner();
        Ok(response.keys)
    }

    /// Revoke an API key by its key_id.
    pub async fn revoke_api_key(&mut self, key_id: impl Into<String>) -> Result<()> {
        let request = Request::new(RevokeApiKeyRequest {
            key_id: key_id.into(),
        });
        let request = self.add_auth_header(request).await?;
        timeout(self.request_timeout, self.client.revoke_api_key(request))
            .await
            .context("revoke_api_key timed out")??;
        Ok(())
    }

    // ─── Helpers ──────────────────────────────────────────────────────────────

    async fn add_auth_header<T>(&self, mut request: Request<T>) -> Result<Request<T>> {
        let user_access_token =
            crate::auth::auth_resolve_user_access_token(Some(&self.user_access_token))
                .await
                .map_err(|err| anyhow::anyhow!(err))?
                .ok_or_else(|| {
                    anyhow::anyhow!("User access token is required for application management RPCs")
                })?;

        crate::auth::auth_insert_bearer_header(&mut request, &user_access_token)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(request)
    }
}
