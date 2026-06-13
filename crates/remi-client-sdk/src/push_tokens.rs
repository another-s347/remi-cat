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
    pub use public_api::v1::{RegisterPushTokenRequest, RegisterPushTokenResponse};
}

use proto::{PublicServiceClient, RegisterPushTokenRequest, RegisterPushTokenResponse};

struct ClientState {
    transport: Arc<SharedTransport>,
    client: Mutex<Option<PublicServiceClient<Channel>>>,
    request_timeout: Duration,
}

impl ClientState {
    async fn register_push_token(
        &self,
        payload: RegisterPushTokenRequest,
    ) -> Result<RegisterPushTokenResponse, String> {
        let mut client_guard = self.client.lock().await;
        if client_guard.is_none() {
            let channel = self.transport.get_channel().await?;
            client_guard.replace(PublicServiceClient::new(channel));
        }

        let client = client_guard
            .as_mut()
            .expect("client must exist after initialization");

        let bearer_token = crate::auth::auth_get_bearer_token()
            .await
            .ok_or_else(|| "Authentication bearer token is not configured".to_string())?;

        let mut request = Request::new(payload);
        crate::auth::auth_insert_bearer_header(&mut request, &bearer_token)?;

        let response = timeout(self.request_timeout, client.register_push_token(request))
            .await
            .map_err(|_| "RegisterPushToken request timed out".to_string())
            .and_then(|result| {
                result.map_err(|err| format!("Failed to register push token: {err}"))
            });

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

static PUSH_TOKEN_CLIENT_STATE: OnceCell<Arc<RwLock<Option<Arc<ClientState>>>>> = OnceCell::new();

fn push_state() -> &'static Arc<RwLock<Option<Arc<ClientState>>>> {
    PUSH_TOKEN_CLIENT_STATE.get_or_init(|| Arc::new(RwLock::new(None)))
}

pub async fn configure_push_token_client(config_json: String) -> Result<(), String> {
    let transport = configure_shared_transport(&config_json).await?;
    let request_timeout = transport.request_timeout();

    let state = Arc::new(ClientState {
        transport,
        client: Mutex::new(None),
        request_timeout,
    });

    let previous = {
        let mut guard = push_state().write().await;
        guard.replace(state.clone())
    };

    if let Some(old) = previous {
        old.drop_client().await;
    }

    Ok(())
}

pub async fn register_push_token(
    device_id: String,
    fcm_token: String,
    provider: Option<String>,
    platform: Option<String>,
    app_version: Option<String>,
) -> Result<String, String> {
    let maybe_state = {
        let guard = push_state().read().await;
        guard.clone()
    };
    let state = maybe_state.ok_or_else(|| "Push token client is not configured".to_string())?;

    let payload = RegisterPushTokenRequest {
        device_id,
        provider: provider.unwrap_or_else(|| "fcm".to_string()),
        platform: platform.unwrap_or_default(),
        fcm_token,
        app_version: app_version.unwrap_or_default(),
    };

    let resp = state.register_push_token(payload).await?;
    Ok(serde_json::json!({
        "status": resp.status,
        "updatedAt": resp.updated_at,
    })
    .to_string())
}
