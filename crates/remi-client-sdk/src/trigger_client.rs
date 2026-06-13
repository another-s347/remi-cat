use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::time::timeout;
use tonic::Request;
use tonic::transport::Channel;

// Include generated proto code
pub mod proto {
    tonic::include_proto!("public_api.v1");
}

use proto::{
    CrdtDocumentRef, GetCrdtDocumentSnapshotRequest, GetCrdtDocumentSnapshotsRequest,
    GetThingsSnapshotRequest, GetThingsSyncStatusRequest, ListCrdtDocumentKeysRequest,
    ListTriggersRequest, ListTriggersResponse, QueryThingsChangeLogsRequest,
    ReportTriggerFiredRequest, SyncCrdtDocumentInput, SyncCrdtDocumentRequest,
    SyncCrdtDocumentsRequest, SyncThingsChangeLogsRequest, SyncThingsChangeLogsResponse,
    SyncThingsRequest, ThingsChangeLogEntry as ProtoThingsChangeLogEntry,
    ThingsContentSnapshot as ProtoThingsContentSnapshot, TriggerInfo, UploadTriggerChunk,
    UploadTriggerResponse, public_service_client::PublicServiceClient,
};

const MAX_GRPC_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

fn configured_public_service_client(channel: Channel) -> PublicServiceClient<Channel> {
    PublicServiceClient::new(channel)
        .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
        .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES)
}

/// Client for exploring and installing triggers from the server
pub struct TriggerClient {
    client: PublicServiceClient<Channel>,
    bearer_token: String,
    request_timeout: Duration,
}

impl TriggerClient {
    /// Create a new trigger client
    pub async fn new(
        server_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self> {
        let channel = Channel::from_shared(server_url.into())
            .context("Invalid server URL")?
            .connect()
            .await
            .context("Failed to connect to server")?;

        let client = configured_public_service_client(channel);

        Ok(Self {
            client,
            bearer_token: bearer_token.into(),
            request_timeout: Duration::from_secs(60),
        })
    }

    /// Create a trigger client that reuses the shared transport configured for auth/telemetry
    pub async fn new_with_shared_transport(bearer_token: impl Into<String>) -> Result<Self> {
        let transport =
            crate::transport::get_shared_transport().map_err(|err| anyhow::anyhow!(err))?;
        let request_timeout = transport.request_timeout();
        let channel = transport
            .get_channel()
            .await
            .map_err(|err| anyhow::anyhow!(err))?;

        let client = configured_public_service_client(channel);

        Ok(Self {
            client,
            bearer_token: bearer_token.into(),
            request_timeout,
        })
    }

    /// List available triggers from the server
    pub async fn list_triggers(
        &mut self,
        device_id: impl Into<String>,
        search_query: Option<String>,
        limit: i32,
        offset: i32,
    ) -> Result<ListTriggersResponse> {
        let request = Request::new(ListTriggersRequest {
            device_id: device_id.into(),
            search_query: search_query.unwrap_or_default(),
            limit,
            offset,
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(self.request_timeout, self.client.list_triggers(request))
            .await
            .context("List triggers timed out")??
            .into_inner();

        Ok(response)
    }

    /// Download a trigger's rule configuration JSON.
    pub async fn download_trigger_rule_config(
        &mut self,
        device_id: impl Into<String>,
        trigger_uuid: impl Into<String>,
    ) -> Result<String> {
        let request = Request::new(proto::GetTriggerRequest {
            device_id: device_id.into(),
            uuid: trigger_uuid.into(),
        });

        let request = self.add_auth_header(request).await?;

        let mut stream = timeout(
            self.request_timeout,
            self.client.get_trigger_stream(request),
        )
        .await
        .context("Trigger download timed out")??
        .into_inner();

        let mut rule_config_json: Option<String> = None;

        while let Some(chunk) = stream.message().await.context("Failed to receive chunk")? {
            if let Some(meta) = chunk.metadata {
                rule_config_json = Some(meta.rule_config_json);
            }
        }

        rule_config_json.context("No metadata received in stream")
    }

    async fn add_auth_header<T>(&self, mut request: Request<T>) -> Result<Request<T>> {
        let bearer_token = crate::auth::auth_resolve_bearer_token(Some(&self.bearer_token))
            .await
            .ok_or_else(|| anyhow::anyhow!("Authentication bearer token is not configured"))?;

        crate::auth::auth_insert_bearer_header(&mut request, &bearer_token)
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(request)
    }

    /// Upload a trigger definition (rule_config_json) to the server.
    ///
    /// This is used by the mobile app to sync locally-installed triggers back to the server.
    /// The server implementation is expected to be idempotent for the same (user_id, uuid).
    pub async fn upload_trigger_rule_config_json(
        &mut self,
        device_id: impl Into<String>,
        trigger_uuid: String,
        name: String,
        version: i32,
        rule_config_json: String,
    ) -> Result<UploadTriggerResponse> {
        let metadata = proto::upload_trigger_chunk::Metadata {
            device_id: device_id.into(),
            uuid: trigger_uuid,
            name,
            user_request: String::new(),
            event_analysis: String::new(),
            version,
            publish_binding: None,
            rule_config_json,
        };

        let chunks = vec![UploadTriggerChunk {
            metadata: Some(metadata),
        }];

        // Convert the in-memory chunks into a tonic streaming request.
        let outbound = tokio_stream::iter(chunks);
        let request = Request::new(outbound);
        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.upload_trigger_stream(request),
        )
        .await
        .context("Upload trigger timed out")??
        .into_inner();

        Ok(response)
    }

    /// Report that a trigger fired on this device so the server can fan-out the event to
    /// all other devices belonging to the same user.
    pub async fn report_trigger_fired(
        &mut self,
        trigger_id: impl Into<String>,
        trigger_name: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Result<()> {
        let request = Request::new(ReportTriggerFiredRequest {
            trigger_id: trigger_id.into(),
            trigger_name: trigger_name.into(),
            device_id: device_id.into(),
        });
        let request = self.add_auth_header(request).await?;
        timeout(
            self.request_timeout,
            self.client.report_trigger_fired(request),
        )
        .await
        .context("ReportTriggerFired timed out")??;
        Ok(())
    }
}

/// Information about a trigger available on the server
#[derive(Debug, Clone)]
pub struct ServerTriggerInfo {
    pub uuid: String,
    pub name: String,
    pub user_request: String,
    pub event_analysis: String,
    pub version: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ServerCrdtDocumentKey {
    pub document_uuid: String,
    pub data_type: i32,
    pub canonical_head: Vec<u8>,
}

#[async_trait]
pub trait CrdtSyncTransport: Send {
    async fn sync_crdt_document(
        &mut self,
        device_id: String,
        document_uuid: String,
        data_type: i32,
        sync_message: Vec<u8>,
    ) -> Result<(Vec<Vec<u8>>, String)>;

    async fn sync_crdt_documents(
        &mut self,
        device_id: String,
        documents: Vec<(String, i32, Vec<u8>)>,
    ) -> Result<Vec<(String, i32, Vec<Vec<u8>>, String)>> {
        let mut responses = Vec::with_capacity(documents.len());
        for (document_uuid, data_type, sync_message) in documents {
            let (sync_messages, last_sync_at) = self
                .sync_crdt_document(
                    device_id.clone(),
                    document_uuid.clone(),
                    data_type,
                    sync_message,
                )
                .await?;
            responses.push((document_uuid, data_type, sync_messages, last_sync_at));
        }
        Ok(responses)
    }

    async fn get_crdt_document_snapshot(
        &mut self,
        device_id: String,
        document_uuid: String,
        data_type: i32,
        reset_sync_state: bool,
    ) -> Result<(Vec<u8>, String)>;

    async fn get_crdt_document_snapshots(
        &mut self,
        device_id: String,
        documents: Vec<(String, i32)>,
        reset_sync_state: bool,
    ) -> Result<Vec<(String, i32, Vec<u8>, String)>> {
        let mut snapshots = Vec::with_capacity(documents.len());
        for (document_uuid, data_type) in documents {
            let (automerge_doc, last_sync_at) = self
                .get_crdt_document_snapshot(
                    device_id.clone(),
                    document_uuid.clone(),
                    data_type,
                    reset_sync_state,
                )
                .await?;
            snapshots.push((document_uuid, data_type, automerge_doc, last_sync_at));
        }
        Ok(snapshots)
    }

    async fn list_crdt_document_keys(&mut self) -> Result<Vec<ServerCrdtDocumentKey>>;
}

impl From<TriggerInfo> for ServerTriggerInfo {
    fn from(info: TriggerInfo) -> Self {
        Self {
            uuid: info.uuid,
            name: info.name,
            user_request: info.user_request,
            event_analysis: info.event_analysis,
            version: info.version,
            created_at: info.created_at,
            updated_at: info.updated_at,
        }
    }
}

#[async_trait]
impl CrdtSyncTransport for TriggerClient {
    async fn sync_crdt_document(
        &mut self,
        device_id: String,
        document_uuid: String,
        data_type: i32,
        sync_message: Vec<u8>,
    ) -> Result<(Vec<Vec<u8>>, String)> {
        TriggerClient::sync_crdt_document(self, device_id, document_uuid, data_type, sync_message)
            .await
    }

    async fn sync_crdt_documents(
        &mut self,
        device_id: String,
        documents: Vec<(String, i32, Vec<u8>)>,
    ) -> Result<Vec<(String, i32, Vec<Vec<u8>>, String)>> {
        TriggerClient::sync_crdt_documents(self, device_id, documents).await
    }

    async fn get_crdt_document_snapshot(
        &mut self,
        device_id: String,
        document_uuid: String,
        data_type: i32,
        reset_sync_state: bool,
    ) -> Result<(Vec<u8>, String)> {
        TriggerClient::get_crdt_document_snapshot(
            self,
            device_id,
            document_uuid,
            data_type,
            reset_sync_state,
        )
        .await
    }

    async fn get_crdt_document_snapshots(
        &mut self,
        device_id: String,
        documents: Vec<(String, i32)>,
        reset_sync_state: bool,
    ) -> Result<Vec<(String, i32, Vec<u8>, String)>> {
        TriggerClient::get_crdt_document_snapshots(self, device_id, documents, reset_sync_state)
            .await
    }

    async fn list_crdt_document_keys(&mut self) -> Result<Vec<ServerCrdtDocumentKey>> {
        TriggerClient::list_crdt_document_keys(self).await
    }
}

impl TriggerClient {
    /// Sync things with the server using automerge sync protocol.
    pub async fn sync_things(
        &mut self,
        device_id: String,
        sync_message: Vec<u8>,
    ) -> Result<(Vec<Vec<u8>>, String)> {
        let request = Request::new(SyncThingsRequest {
            device_id,
            sync_message,
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(self.request_timeout, self.client.sync_things(request))
            .await
            .context("Sync things timed out")??
            .into_inner();

        Ok((response.sync_messages, response.last_sync_at))
    }

    // ========== CRDT V3 Multi-Document Sync ==========

    /// Sync a single CRDT document with the server.
    pub async fn sync_crdt_document(
        &mut self,
        device_id: String,
        document_uuid: String,
        data_type: i32,
        sync_message: Vec<u8>,
    ) -> Result<(Vec<Vec<u8>>, String)> {
        let request = Request::new(SyncCrdtDocumentRequest {
            device_id,
            document_uuid,
            data_type,
            sync_message,
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.sync_crdt_document(request),
        )
        .await
        .context("Sync CRDT document timed out")??
        .into_inner();

        Ok((response.sync_messages, response.last_sync_at))
    }

    /// Sync multiple CRDT documents with the server in a single roundtrip.
    pub async fn sync_crdt_documents(
        &mut self,
        device_id: String,
        documents: Vec<(String, i32, Vec<u8>)>,
    ) -> Result<Vec<(String, i32, Vec<Vec<u8>>, String)>> {
        let request = Request::new(SyncCrdtDocumentsRequest {
            device_id,
            documents: documents
                .into_iter()
                .map(
                    |(document_uuid, data_type, sync_message)| SyncCrdtDocumentInput {
                        document_uuid,
                        data_type,
                        sync_message,
                    },
                )
                .collect(),
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.sync_crdt_documents(request),
        )
        .await
        .context("Sync CRDT documents timed out")??
        .into_inner();

        Ok(response
            .documents
            .into_iter()
            .map(|document| {
                (
                    document.document_uuid,
                    document.data_type,
                    document.sync_messages,
                    document.last_sync_at,
                )
            })
            .collect())
    }

    /// Fetch the latest CRDT document snapshot (for bootstrap).
    pub async fn get_crdt_document_snapshot(
        &mut self,
        device_id: String,
        document_uuid: String,
        data_type: i32,
        reset_sync_state: bool,
    ) -> Result<(Vec<u8>, String)> {
        let request = Request::new(GetCrdtDocumentSnapshotRequest {
            device_id,
            document_uuid,
            data_type,
            reset_sync_state,
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.get_crdt_document_snapshot(request),
        )
        .await
        .context("Get CRDT document snapshot timed out")??
        .into_inner();

        Ok((response.automerge_doc, response.last_sync_at))
    }

    /// Fetch multiple CRDT document snapshots in one request.
    pub async fn get_crdt_document_snapshots(
        &mut self,
        device_id: String,
        documents: Vec<(String, i32)>,
        reset_sync_state: bool,
    ) -> Result<Vec<(String, i32, Vec<u8>, String)>> {
        let request = Request::new(GetCrdtDocumentSnapshotsRequest {
            device_id,
            documents: documents
                .into_iter()
                .map(|(document_uuid, data_type)| CrdtDocumentRef {
                    document_uuid,
                    data_type,
                })
                .collect(),
            reset_sync_state,
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.get_crdt_document_snapshots(request),
        )
        .await
        .context("Get CRDT document snapshots timed out")??
        .into_inner();

        Ok(response
            .snapshots
            .into_iter()
            .map(|snapshot| {
                (
                    snapshot.document_uuid,
                    snapshot.data_type,
                    snapshot.automerge_doc,
                    snapshot.last_sync_at,
                )
            })
            .collect())
    }

    /// List all CRDT document keys for the user, including the current canonical server head.
    pub async fn list_crdt_document_keys(&mut self) -> Result<Vec<ServerCrdtDocumentKey>> {
        let request = Request::new(ListCrdtDocumentKeysRequest {});

        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.list_crdt_document_keys(request),
        )
        .await
        .context("List CRDT document keys timed out")??
        .into_inner();

        Ok(response
            .keys
            .into_iter()
            .map(|k| ServerCrdtDocumentKey {
                document_uuid: k.document_uuid,
                data_type: k.data_type,
                canonical_head: k.canonical_head,
            })
            .collect())
    }

    /// Fetch the latest things Automerge document (snapshot/bootstrap).
    pub async fn get_things_snapshot(
        &mut self,
        device_id: String,
        reset_sync_state: bool,
    ) -> Result<(Vec<u8>, String)> {
        let request = Request::new(GetThingsSnapshotRequest {
            device_id,
            reset_sync_state,
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.get_things_snapshot(request),
        )
        .await
        .context("Get things snapshot timed out")??
        .into_inner();

        Ok((response.automerge_doc, response.last_sync_at))
    }

    /// Query sync status for things.
    pub async fn get_things_sync_status(
        &mut self,
        device_id: impl Into<String>,
        last_synced_server_head: Vec<u8>,
    ) -> Result<proto::GetThingsSyncStatusResponse> {
        let request = Request::new(GetThingsSyncStatusRequest {
            device_id: device_id.into(),
            last_synced_server_head,
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.get_things_sync_status(request),
        )
        .await
        .context("Get things sync status timed out")??
        .into_inner();

        Ok(response)
    }

    /// Sync change logs and content snapshots with the server.
    pub async fn sync_things_change_logs(
        &mut self,
        device_id: impl Into<String>,
        upload_change_logs: Vec<ProtoThingsChangeLogEntry>,
        upload_snapshots: Vec<ProtoThingsContentSnapshot>,
        last_synced_log_id: i64,
        last_synced_snapshot_id: i64,
    ) -> Result<SyncThingsChangeLogsResponse> {
        let request = Request::new(SyncThingsChangeLogsRequest {
            device_id: device_id.into(),
            upload_change_logs,
            upload_snapshots,
            last_synced_log_id,
            last_synced_snapshot_id,
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.sync_things_change_logs(request),
        )
        .await
        .context("Sync things change logs timed out")??
        .into_inner();

        Ok(response)
    }

    /// Query change logs from the server with filters.
    pub async fn query_things_change_logs(
        &mut self,
        device_id: impl Into<String>,
        filter_device: Option<String>,
        filter_entity: Option<String>,
        from_timestamp: Option<i64>,
        to_timestamp: Option<i64>,
        limit: i32,
        offset: i32,
    ) -> Result<proto::QueryThingsChangeLogsResponse> {
        let request = Request::new(QueryThingsChangeLogsRequest {
            device_id: device_id.into(),
            filter_device: filter_device.unwrap_or_default(),
            filter_entity: filter_entity.unwrap_or_default(),
            from_timestamp: from_timestamp.unwrap_or(0),
            to_timestamp: to_timestamp.unwrap_or(0),
            limit,
            offset,
        });

        let request = self.add_auth_header(request).await?;

        let response = timeout(
            self.request_timeout,
            self.client.query_things_change_logs(request),
        )
        .await
        .context("Query things change logs timed out")??
        .into_inner();

        Ok(response)
    }
}
