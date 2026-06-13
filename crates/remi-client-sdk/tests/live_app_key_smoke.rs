use std::env;

use anyhow::{Context, Result};
use remi_client_sdk::app_keys_client::{self, AppKeysClient};
use remi_client_sdk::auth::{
    auth_clear_app_key, auth_get_bearer_auth_mode, auth_insert_bearer_header, auth_set_app_key,
};
use remi_client_sdk::transport::{configure_shared_transport, get_shared_transport};
use remi_client_sdk::{SdkBearerAuthMode, TriggerClient};
use serde_json::json;

fn live_transport_config_json() -> String {
    let grpc_addr =
        env::var("REMI_PUBLIC_GRPC_ADDR").unwrap_or_else(|_| "127.0.0.1:50051".to_string());

    json!({
        "transportMode": "tcp",
        "tcpGrpcAddr": grpc_addr,
        "connectTimeoutMs": 3000,
        "requestTimeoutMs": 10000,
    })
    .to_string()
}

#[tokio::test]
#[ignore = "requires a live backend plus REMI_LIVE_APP_KEY"]
async fn live_app_key_smoke() -> Result<()> {
    let app_key = env::var("REMI_LIVE_APP_KEY")
        .context("REMI_LIVE_APP_KEY must be set for live app-key smoke tests")?;

    let config_json = live_transport_config_json();
    configure_shared_transport(&config_json)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;

    auth_clear_app_key().await;
    auth_set_app_key(app_key.clone())
        .await
        .map_err(|err| anyhow::anyhow!(err))?;

    assert_eq!(
        auth_get_bearer_auth_mode().await,
        Some(SdkBearerAuthMode::AppKey)
    );

    // Positive path: business RPCs should work in app-key-only mode.
    let device_id = format!("copilot-app-key-smoke-{}", std::process::id());
    let mut trigger_client = TriggerClient::new_with_shared_transport(String::new())
        .await
        .context("failed to create TriggerClient")?;

    let response = trigger_client
        .list_triggers(device_id, None, 1, 0)
        .await
        .context("app key should be accepted for business RPCs")?;

    assert!(response.total_count >= 0);

    // SDK boundary: application management helpers should reject app keys before the call.
    let mut app_keys_client = AppKeysClient::new_with_shared_transport(app_key.clone())
        .await
        .context("failed to create AppKeysClient")?;
    let sdk_err = app_keys_client
        .list_applications()
        .await
        .expect_err("AppKeysClient must reject app keys for application management RPCs");
    assert!(
        sdk_err
            .to_string()
            .contains("Application API keys cannot be used for application management RPCs")
    );

    // Server boundary: direct gRPC access with an app key must also be rejected.
    let transport = get_shared_transport().map_err(|err| anyhow::anyhow!(err))?;
    let channel = transport
        .get_channel()
        .await
        .map_err(|err| anyhow::anyhow!(err))?;
    let mut raw_client =
        app_keys_client::proto::public_service_client::PublicServiceClient::new(channel);
    let mut request = tonic::Request::new(app_keys_client::proto::ListApplicationsRequest {});
    auth_insert_bearer_header(&mut request, &app_key).map_err(|err| anyhow::anyhow!(err))?;

    let server_err = raw_client
        .list_applications(request)
        .await
        .expect_err("server must reject app keys for application management RPCs");
    assert_eq!(server_err.code(), tonic::Code::PermissionDenied);

    auth_clear_app_key().await;

    Ok(())
}
