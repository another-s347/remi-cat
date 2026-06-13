use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::timeout;
use tonic::Request;
use tonic::transport::Channel;

pub mod proto {
    tonic::include_proto!("public_api.v1");
}

use proto::{
    CreateThingCollectionRequest, CreateThingCollectionResponse, CreateThingRequest,
    CreateThingResponse, GetThingCollectionRequest, GetThingCollectionResponse, GetThingRequest,
    GetThingResponse, ListThingCollectionsRequest, ListThingCollectionsResponse, ListThingsRequest,
    ListThingsResponse, UpdateThingCollectionRequest, UpdateThingCollectionResponse,
    UpdateThingRequest, UpdateThingResponse, public_service_client::PublicServiceClient,
};

pub struct ThingsClient {
    client: PublicServiceClient<Channel>,
    bearer_token: String,
    request_timeout: Duration,
}

impl ThingsClient {
    pub async fn new(
        server_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self> {
        let channel = Channel::from_shared(server_url.into())
            .context("Invalid server URL")?
            .connect()
            .await
            .context("Failed to connect to server")?;

        Ok(Self {
            client: PublicServiceClient::new(channel),
            bearer_token: bearer_token.into(),
            request_timeout: Duration::from_secs(60),
        })
    }

    pub async fn new_with_shared_transport(bearer_token: impl Into<String>) -> Result<Self> {
        let transport =
            crate::transport::get_shared_transport().map_err(|err| anyhow::anyhow!(err))?;
        let request_timeout = transport.request_timeout();
        let channel = transport
            .get_channel()
            .await
            .map_err(|err| anyhow::anyhow!(err))?;

        Ok(Self {
            client: PublicServiceClient::new(channel),
            bearer_token: bearer_token.into(),
            request_timeout,
        })
    }

    pub async fn create_collection(
        &mut self,
        title: impl Into<String>,
        uuid: Option<String>,
    ) -> Result<CreateThingCollectionResponse> {
        let request = self
            .add_auth_header(Request::new(CreateThingCollectionRequest {
                uuid: uuid.unwrap_or_default(),
                title: title.into(),
            }))
            .await?;

        let response = timeout(
            self.request_timeout,
            self.client.create_thing_collection(request),
        )
        .await
        .context("CreateThingCollection request timed out")??;
        Ok(response.into_inner())
    }

    pub async fn list_collections(
        &mut self,
        limit: i32,
        offset: i32,
        creator_app_id: Option<String>,
    ) -> Result<ListThingCollectionsResponse> {
        let request = self
            .add_auth_header(Request::new(ListThingCollectionsRequest {
                limit,
                offset,
                creator_app_id: creator_app_id.unwrap_or_default(),
            }))
            .await?;

        let response = timeout(
            self.request_timeout,
            self.client.list_thing_collections(request),
        )
        .await
        .context("ListThingCollections request timed out")??;
        Ok(response.into_inner())
    }

    pub async fn get_collection(
        &mut self,
        uuid: impl Into<String>,
    ) -> Result<GetThingCollectionResponse> {
        let request = self
            .add_auth_header(Request::new(GetThingCollectionRequest {
                uuid: uuid.into(),
            }))
            .await?;

        let response = timeout(
            self.request_timeout,
            self.client.get_thing_collection(request),
        )
        .await
        .context("GetThingCollection request timed out")??;
        Ok(response.into_inner())
    }

    pub async fn update_collection(
        &mut self,
        uuid: impl Into<String>,
        title: impl Into<String>,
    ) -> Result<UpdateThingCollectionResponse> {
        let request = self
            .add_auth_header(Request::new(UpdateThingCollectionRequest {
                uuid: uuid.into(),
                title: title.into(),
            }))
            .await?;

        let response = timeout(
            self.request_timeout,
            self.client.update_thing_collection(request),
        )
        .await
        .context("UpdateThingCollection request timed out")??;
        Ok(response.into_inner())
    }

    pub async fn create_thing(
        &mut self,
        collection_uuid: impl Into<String>,
        datatype: impl Into<String>,
        data_json: impl Into<String>,
        parent_uuid: Option<String>,
        title: Option<String>,
        uuid: Option<String>,
    ) -> Result<CreateThingResponse> {
        let request = self
            .add_auth_header(Request::new(CreateThingRequest {
                uuid: uuid.unwrap_or_default(),
                datatype: datatype.into(),
                data_json: data_json.into(),
                parent_uuid: parent_uuid.unwrap_or_default(),
                collection_uuid: collection_uuid.into(),
                title: title.unwrap_or_default(),
            }))
            .await?;

        let response = timeout(self.request_timeout, self.client.create_thing(request))
            .await
            .context("CreateThing request timed out")??;
        Ok(response.into_inner())
    }

    pub async fn list_things(
        &mut self,
        collection_uuid: Option<String>,
        parent_uuid: Option<String>,
        datatype: Option<String>,
        creator_app_id: Option<String>,
        limit: i32,
        offset: i32,
    ) -> Result<ListThingsResponse> {
        let request = self
            .add_auth_header(Request::new(ListThingsRequest {
                collection_uuid: collection_uuid.unwrap_or_default(),
                parent_uuid: parent_uuid.unwrap_or_default(),
                datatype: datatype.unwrap_or_default(),
                limit,
                offset,
                creator_app_id: creator_app_id.unwrap_or_default(),
            }))
            .await?;

        let response = timeout(self.request_timeout, self.client.list_things(request))
            .await
            .context("ListThings request timed out")??;
        Ok(response.into_inner())
    }

    pub async fn get_thing(&mut self, uuid: impl Into<String>) -> Result<GetThingResponse> {
        let request = self
            .add_auth_header(Request::new(GetThingRequest { uuid: uuid.into() }))
            .await?;

        let response = timeout(self.request_timeout, self.client.get_thing(request))
            .await
            .context("GetThing request timed out")??;
        Ok(response.into_inner())
    }

    pub async fn update_thing(
        &mut self,
        uuid: impl Into<String>,
        datatype: Option<String>,
        data_json: Option<String>,
        parent_uuid: Option<String>,
        collection_uuid: Option<String>,
        title: Option<String>,
    ) -> Result<UpdateThingResponse> {
        let request = self
            .add_auth_header(Request::new(UpdateThingRequest {
                uuid: uuid.into(),
                datatype: datatype.unwrap_or_default(),
                data_json: data_json.unwrap_or_default(),
                parent_uuid: parent_uuid.unwrap_or_default(),
                collection_uuid: collection_uuid.unwrap_or_default(),
                title: title.unwrap_or_default(),
            }))
            .await?;

        let response = timeout(self.request_timeout, self.client.update_thing(request))
            .await
            .context("UpdateThing request timed out")??;
        Ok(response.into_inner())
    }

    async fn add_auth_header<T>(&self, mut request: Request<T>) -> Result<Request<T>> {
        let bearer_token = crate::auth::auth_resolve_bearer_token(Some(&self.bearer_token))
            .await
            .ok_or_else(|| anyhow::anyhow!("Authentication bearer token is not configured"))?;

        crate::auth::auth_insert_bearer_header(&mut request, &bearer_token)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(request)
    }
}

/// Fetch actor attribution metadata from the server for all things & collections belonging
/// to the authenticated user, and cache it locally in the SDK storage.
///
/// Call this after a successful CRDT sync to keep the local attribution cache fresh.
/// Uses the user bearer token from the SDK auth state.
pub async fn refresh_things_actor_meta(sdk: &crate::TriggerSdk) -> Result<()> {
    let bearer_token = crate::auth::auth_get_bearer_token()
        .await
        .context("No bearer token available for actor meta refresh")?;

    let mut client = ThingsClient::new_with_shared_transport(bearer_token).await?;

    let mut entries: Vec<crate::storage::ActorMetaEntry> = Vec::new();

    // Fetch all collections (up to 500, which is sufficient for typical users).
    match client.list_collections(500, 0, None).await {
        Ok(resp) => {
            for col in resp.collections {
                let actor_type = actor_type_str(col.creator.as_ref().map(|a| a.actor_type));
                let actor_app_id = col.creator.as_ref().and_then(|a| {
                    if a.app_id.is_empty() {
                        None
                    } else {
                        Some(a.app_id.clone())
                    }
                });
                let actor_display_name = col.creator.as_ref().and_then(|a| {
                    if a.app_name.is_empty() {
                        None
                    } else {
                        Some(a.app_name.clone())
                    }
                });
                entries.push(crate::storage::ActorMetaEntry {
                    uuid: col.uuid,
                    is_collection: true,
                    actor_type,
                    actor_app_id,
                    actor_display_name,
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to fetch collection actor meta from server");
        }
    }

    // Fetch all things (up to 500 at a time; expand with pagination if needed).
    match client.list_things(None, None, None, None, 500, 0).await {
        Ok(resp) => {
            for thing in resp.things {
                let actor_type = actor_type_str(thing.creator.as_ref().map(|a| a.actor_type));
                let actor_app_id = thing.creator.as_ref().and_then(|a| {
                    if a.app_id.is_empty() {
                        None
                    } else {
                        Some(a.app_id.clone())
                    }
                });
                let actor_display_name = thing.creator.as_ref().and_then(|a| {
                    if a.app_name.is_empty() {
                        None
                    } else {
                        Some(a.app_name.clone())
                    }
                });
                entries.push(crate::storage::ActorMetaEntry {
                    uuid: thing.uuid,
                    is_collection: false,
                    actor_type,
                    actor_app_id,
                    actor_display_name,
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to fetch thing actor meta from server");
        }
    }

    if !entries.is_empty() {
        sdk.things_upsert_actor_meta(&entries)?;
    }

    tracing::debug!(count = entries.len(), "Refreshed things actor meta cache");
    Ok(())
}

/// Convert proto ActorType enum i32 to a display string.
fn actor_type_str(actor_type: Option<i32>) -> String {
    match actor_type {
        Some(1) => "application".to_string(),
        _ => "user".to_string(),
    }
}
