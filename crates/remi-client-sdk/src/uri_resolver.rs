//! URI Resolution — calls the server's ResolveUri RPC and (optionally) writes
//! the resolved metadata back into the CRDT ContentEntry.
//!
//! Two consumption paths:
//! - `resolve_uri`: pure RPC call, returns structured metadata.
//! - `resolve_and_update_entry`: RPC + CRDT writeback (fire-and-forget from Flutter).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::time::timeout;
use tracing::info;

mod proto {
    pub mod public_api {
        pub mod v1 {
            tonic::include_proto!("public_api.v1");
        }
    }
}

use proto::public_api::v1::{ResolveUriRequest, public_service_client::PublicServiceClient};

use crate::auth::auth_get_bearer_token;
use crate::things_crdt::{ContentEntryPayload, ContentEntryUpdate, ImageField, UrlField};
use crate::transport::get_shared_transport;

/// Structured URI metadata returned from the server.
/// Mirrors the proto `UriMetadata` but owned for Rust consumption.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UriMetadata {
    pub original_uri: String,
    pub title: String,
    pub description: String,
    pub image_url: String,
    pub favicon_url: String,
    pub site_name: String,
    pub content_type: String,
    pub is_image: bool,
    pub resolved: bool,
}

/// Call the server's `ResolveUri` RPC and return structured metadata.
///
/// `uri_type` is a hint — typically `"url"` for web pages, `"image"` for
/// direct image URLs.
pub async fn resolve_uri(uri: &str, uri_type: &str) -> Result<UriMetadata> {
    let transport = get_shared_transport().map_err(|e| anyhow::anyhow!(e))?;
    let request_timeout = transport.request_timeout();
    let channel = transport
        .get_channel()
        .await
        .map_err(|e| anyhow::anyhow!(e))?;

    let mut client = PublicServiceClient::new(channel);

    // Build gRPC request (optional auth — ResolveUri may or may not require it)
    let mut req = tonic::Request::new(ResolveUriRequest {
        uri: uri.to_string(),
        uri_type: uri_type.to_string(),
    });

    // Attach auth token if available (best-effort; server may allow unauthenticated)
    if let Some(bearer_token) = auth_get_bearer_token().await {
        crate::auth::auth_insert_bearer_header(&mut req, &bearer_token)
            .map_err(|err| anyhow::anyhow!(err))?;
    }

    let response = timeout(request_timeout, client.resolve_uri(req))
        .await
        .map_err(|_| anyhow::anyhow!("ResolveUri timed out after {:?}", request_timeout))?
        .context("ResolveUri RPC failed")?
        .into_inner();

    if !response.success {
        anyhow::bail!("ResolveUri returned error: {}", response.error);
    }

    let m = response
        .metadata
        .ok_or_else(|| anyhow::anyhow!("ResolveUri returned no metadata"))?;

    Ok(UriMetadata {
        original_uri: m.original_uri,
        title: m.title,
        description: m.description,
        image_url: m.image_url,
        favicon_url: m.favicon_url,
        site_name: m.site_name,
        content_type: m.content_type,
        is_image: m.is_image,
        resolved: m.resolved,
    })
}

/// Resolve a URL via the server and write the result back into the CRDT
/// ContentEntry (fire-and-forget).
///
/// If the URL resolves to an image (Content-Type: image/*), the entry's
/// payload is replaced with an Image payload. Otherwise the Url payload
/// is updated in-place with the resolved metadata.
///
/// This function is designed to be called via `tokio::spawn()` from the
/// Flutter/FRB layer so it doesn't block the caller.
pub async fn resolve_and_update_entry(
    sdk: Arc<crate::TriggerSdk>,
    device_id: String,
    thing_uuid: String,
    entry_id: String,
    url: String,
) -> Result<()> {
    info!(url = %url, entry_id = %entry_id, "Resolving URL for content entry");

    let meta = resolve_uri(&url, "url").await?;

    // Build the update payload
    let payload = if meta.is_image {
        info!(url = %url, "URL is an image, converting to Image payload");
        ContentEntryPayload::Image(ImageField::new(url.clone()))
    } else {
        ContentEntryPayload::Url(UrlField::with_metadata(
            url.clone(),
            (!meta.title.is_empty()).then(|| meta.title.clone()),
            (!meta.description.is_empty()).then(|| meta.description.clone()),
            (!meta.image_url.is_empty()).then(|| meta.image_url.clone()),
            (!meta.favicon_url.is_empty()).then(|| meta.favicon_url.clone()),
            (!meta.site_name.is_empty()).then(|| meta.site_name.clone()),
        ))
    };

    // Write back to CRDT (synchronous op — run on blocking thread)
    let sdk_clone = sdk.clone();
    let device_id_clone = device_id.clone();
    let thing_uuid_clone = thing_uuid.clone();
    let entry_id_clone = entry_id.clone();

    tokio::task::spawn_blocking(move || {
        sdk_clone.things_update_content_entry(
            &device_id_clone,
            &thing_uuid_clone,
            ContentEntryUpdate {
                id: entry_id_clone,
                title: None,
                order: None,
                payload: Some(payload),
            },
        )
    })
    .await
    .context("spawn_blocking join failed")?
    .context("CRDT update failed")?;

    info!(url = %url, entry_id = %entry_id, "URL resolved and CRDT updated");
    Ok(())
}
