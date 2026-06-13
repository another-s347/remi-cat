use std::{sync::Arc, time::Duration};

use once_cell::sync::OnceCell;
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use tonic::Request;
use tonic::transport::Channel;

use crate::transport::{SharedTransport, configure_shared_transport};

pub mod proto {
    pub mod public_api {
        pub mod v1 {
            tonic::include_proto!("public_api.v1");
        }
    }

    pub use public_api::v1::public_service_client::PublicServiceClient;
    pub use public_api::v1::{GetAppUpdateRequest, GetAppUpdateResponse};
}

use proto::{GetAppUpdateRequest, GetAppUpdateResponse, PublicServiceClient};

struct ClientState {
    transport: Arc<SharedTransport>,
    client: Mutex<Option<PublicServiceClient<Channel>>>,
    request_timeout: Duration,
}

impl ClientState {
    async fn get_app_update(
        &self,
        payload: GetAppUpdateRequest,
    ) -> Result<GetAppUpdateResponse, String> {
        let mut client_guard = self.client.lock().await;
        if client_guard.is_none() {
            let channel = self.transport.get_channel().await?;
            client_guard.replace(PublicServiceClient::new(channel));
        }

        let client = client_guard
            .as_mut()
            .expect("client must exist after initialization");

        let request = Request::new(payload);

        let response = timeout(self.request_timeout, client.get_app_update(request))
            .await
            .map_err(|_| "GetAppUpdate request timed out".to_string())
            .and_then(|result| result.map_err(|err| format!("GetAppUpdate failed: {err}")));

        match response {
            Ok(resp) => Ok(resp.into_inner()),
            Err(err) => {
                client_guard.take();
                Err(err)
            }
        }
    }

    async fn drop_client(&self) {
        let mut guard = self.client.lock().await;
        guard.take();
    }
}

static APP_UPDATE_CLIENT_STATE: OnceCell<Arc<RwLock<Option<Arc<ClientState>>>>> = OnceCell::new();

fn state_cell() -> &'static Arc<RwLock<Option<Arc<ClientState>>>> {
    APP_UPDATE_CLIENT_STATE.get_or_init(|| Arc::new(RwLock::new(None)))
}

pub async fn configure_app_update_client(config_json: String) -> Result<(), String> {
    let transport = configure_shared_transport(&config_json).await?;
    let request_timeout = transport.request_timeout();

    let state = Arc::new(ClientState {
        transport,
        client: Mutex::new(None),
        request_timeout,
    });

    let previous = {
        let mut guard = state_cell().write().await;
        guard.replace(state.clone())
    };

    if let Some(old) = previous {
        old.drop_client().await;
    }

    Ok(())
}

pub async fn get_app_update(
    platform: String,
    arch: Option<String>,
    flavor: String,
    current_version_name: Option<String>,
    current_version_code: Option<i32>,
) -> Result<String, String> {
    let maybe_state = {
        let guard = state_cell().read().await;
        guard.clone()
    };
    let state = maybe_state.ok_or_else(|| "App update client is not configured".to_string())?;

    let payload = GetAppUpdateRequest {
        platform,
        arch: arch.unwrap_or_default(),
        flavor,
        current_version_name: current_version_name.unwrap_or_default(),
        current_version_code: current_version_code.unwrap_or(0),
    };

    let resp = state.get_app_update(payload).await?;

    Ok(serde_json::json!({
        "updateAvailable": resp.update_available,
        "latestVersionName": resp.latest_version_name,
        "latestVersionCode": resp.latest_version_code,
        "downloadUrl": resp.download_url,
        "sha256": resp.sha256,
        "sizeBytes": resp.size_bytes,
        "releaseNotes": resp.release_notes,
        "forceUpdate": resp.force_update,
        "channel": resp.channel,
    })
    .to_string())
}
