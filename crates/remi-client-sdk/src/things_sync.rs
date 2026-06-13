use std::time::Instant;

use anyhow::{Context, Result};

use crate::TriggerClient;
use crate::TriggerSdk;
use crate::crdt_sync;
use crate::things_crdt::{DocumentPersistence, ThingsDocumentSet};
use crate::trigger_client::{CrdtSyncTransport, ServerCrdtDocumentKey};

use remi_things_crdt::CrdtDataType;

struct ThingsSyncOutput {
    pub doc_bytes: Vec<u8>,
    pub sync_state_bytes: Vec<u8>,
    pub last_sync_at: Option<String>,
    pub rpc_rounds: usize,
    pub server_reply_messages: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalBootstrapState {
    Empty,
    DirtyUnsynced,
    HasSyncedHistory,
}

// ============================================================================
// V3 Multi-Document Batched Sync
// ============================================================================

/// Output from v3 batched sync
pub struct ThingsV3SyncMetrics {
    pub total_elapsed_ms: u64,
    pub bootstrap_pull_ms: u64,
    pub phase1_push_ms: u64,
    pub phase1b_receive_ms: u64,
    pub phase2_pull_ms: u64,
    pub list_keys_calls: usize,
    pub snapshot_downloads: usize,
    pub phase1_documents_synced: usize,
    pub phase1b_documents_synced: usize,
    pub phase2_documents_synced: usize,
    pub phase1_rpc_rounds: usize,
    pub phase1b_rpc_rounds: usize,
    pub phase1_batch_calls: usize,
    pub phase1b_batch_calls: usize,
    pub phase1_server_reply_messages: usize,
    pub phase1b_server_reply_messages: usize,
}

pub struct ThingsV3SyncOutput {
    /// Number of documents synced
    pub documents_synced: usize,
    /// Last sync timestamp (from server)
    pub last_sync_at: Option<String>,
    /// Timing and work-distribution metrics for the sync run.
    pub metrics: ThingsV3SyncMetrics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThingsSyncMode {
    Incremental,
    Full,
}

impl ThingsSyncMode {
    fn allows_server_discovery(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// Convert CrdtDataType to proto enum value
fn data_type_to_proto(data_type: &CrdtDataType) -> i32 {
    match data_type {
        CrdtDataType::Root => 1,
        CrdtDataType::Collection => 2,
        CrdtDataType::ThingMarkdown => 3,
    }
}

fn parse_row_data_type(data_type: &str) -> Result<CrdtDataType> {
    match data_type {
        "root" => Ok(CrdtDataType::Root),
        "collection" => Ok(CrdtDataType::Collection),
        "thing_markdown" => Ok(CrdtDataType::ThingMarkdown),
        _ => anyhow::bail!("Unknown CRDT data type: {data_type}"),
    }
}

fn local_canonical_head(doc_bytes: &[u8]) -> Option<Vec<u8>> {
    let mut doc = automerge::AutoCommit::load(doc_bytes).ok()?;
    let heads = doc.get_heads();
    if heads.len() != 1 {
        return None;
    }

    Some(heads[0].as_ref().to_vec())
}

fn document_is_at_server_head(doc_bytes: &[u8], server_head: &[u8]) -> bool {
    if server_head.is_empty() {
        return false;
    }

    local_canonical_head(doc_bytes)
        .as_deref()
        .map(|local_head| local_head == server_head)
        .unwrap_or(false)
}

fn build_server_head_map(
    server_keys: &[ServerCrdtDocumentKey],
) -> std::collections::HashMap<(String, String), Vec<u8>> {
    let mut out = std::collections::HashMap::new();

    for key in server_keys {
        let dt_str = proto_data_type_to_str(key.data_type);
        if dt_str.is_empty() || key.canonical_head.is_empty() {
            continue;
        }

        out.insert(
            (key.document_uuid.clone(), dt_str.to_string()),
            key.canonical_head.clone(),
        );
    }

    out
}

fn has_sync_history(doc: &crate::types::CrdtDocumentRow) -> bool {
    let initial_sync_state = crate::crdt_sync::init_sync_state();
    !doc.sync_state.is_empty() && doc.sync_state != initial_sync_state
}

fn optional_sync_timestamp(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn observe_sync_timestamp(observed: &mut Option<String>, candidate: Option<String>) {
    if let Some(ts) = candidate {
        *observed = Some(ts);
    }
}

fn classify_local_bootstrap_state(
    sdk: &TriggerSdk,
    dirty_docs: &[crate::types::CrdtDocumentRow],
) -> LocalBootstrapState {
    if dirty_docs.iter().any(has_sync_history) {
        return LocalBootstrapState::HasSyncedHistory;
    }

    let all_keys = sdk.crdt_list_document_keys().unwrap_or_default();
    if all_keys.is_empty() {
        return LocalBootstrapState::Empty;
    }

    let has_synced_history = all_keys.iter().any(|(uuid, dt)| {
        sdk.crdt_get_document(uuid, dt)
            .ok()
            .flatten()
            .map(|doc| has_sync_history(&doc))
            .unwrap_or(false)
    });

    if has_synced_history {
        LocalBootstrapState::HasSyncedHistory
    } else {
        LocalBootstrapState::DirtyUnsynced
    }
}

fn never_synced_dirty_keys(dirty_docs: &[crate::types::CrdtDocumentRow]) -> Vec<(String, String)> {
    dirty_docs
        .iter()
        .filter(|doc| !has_sync_history(doc))
        .map(|doc| (doc.uuid.clone(), doc.data_type.clone()))
        .collect()
}

enum ServerKeyDiscovery {
    Available(Vec<ServerCrdtDocumentKey>),
    Unavailable,
}

impl ServerKeyDiscovery {
    fn keys(&self) -> Option<&[ServerCrdtDocumentKey]> {
        match self {
            Self::Available(keys) => Some(keys),
            Self::Unavailable => None,
        }
    }
}

#[derive(Default)]
struct LocalReachabilityFilters {
    active_collections: Option<std::collections::HashSet<String>>,
    active_things: Option<std::collections::HashSet<String>>,
    active_content_documents: Option<std::collections::HashSet<String>>,
}

struct PullMissingDocumentsOutput {
    documents_pulled: usize,
    last_sync_at: Option<String>,
    snapshot_downloads: usize,
    list_keys_calls: usize,
}

fn build_local_reachability_filters(
    sdk: &TriggerSdk,
    device_id: &str,
) -> Result<LocalReachabilityFilters> {
    let has_synced_non_root_documents = sdk
        .crdt_list_document_keys()
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, data_type)| data_type != "root")
        .any(|(uuid, data_type)| {
            sdk.crdt_get_document(&uuid, &data_type)
                .ok()
                .flatten()
                .map(|row| has_sync_history(&row))
                .unwrap_or(false)
        });

    if !has_synced_non_root_documents {
        return Ok(LocalReachabilityFilters::default());
    }

    let doc_set = load_document_set_from_storage(sdk, device_id)?;
    Ok(LocalReachabilityFilters {
        active_collections: Some(doc_set.active_collection_uuids()?),
        active_things: Some(doc_set.active_thing_uuids()?),
        active_content_documents: Some(doc_set.active_content_document_uuids()?),
    })
}

fn clean_document_should_receive(
    uuid: &str,
    data_type_str: &str,
    doc_bytes: &[u8],
    filters: &LocalReachabilityFilters,
) -> bool {
    match data_type_str {
        "collection" => match &filters.active_collections {
            Some(active) if !active.contains(uuid) => {
                remi_things_crdt::extract_collection_doc_view(doc_bytes, uuid)
                    .map(|view| {
                        !view
                            .meta
                            .tombstone
                            .as_ref()
                            .map(|t| t.deleted)
                            .unwrap_or(false)
                    })
                    .unwrap_or(true)
            }
            _ => true,
        },
        "thing_markdown" => filters
            .active_things
            .as_ref()
            .zip(filters.active_content_documents.as_ref())
            .map(|(active_things, active_content_documents)| {
                active_things.contains(uuid) || active_content_documents.contains(uuid)
            })
            .unwrap_or(true),
        _ => true,
    }
}

fn should_pull_missing_document(
    uuid: &str,
    data_type_str: &str,
    filters: &LocalReachabilityFilters,
) -> bool {
    match data_type_str {
        // Collections are top-level discoverable documents. Filtering them by
        // the current local root index prevents devices from learning about
        // collections created elsewhere after both sides already have sync
        // history.
        "collection" => true,
        "thing_markdown" => filters
            .active_things
            .as_ref()
            .zip(filters.active_content_documents.as_ref())
            .map(|(active_things, active_content_documents)| {
                active_things.contains(uuid) || active_content_documents.contains(uuid)
            })
            .unwrap_or(true),
        _ => true,
    }
}

fn update_reachability_from_downloaded_doc(
    uuid: &str,
    data_type_str: &str,
    doc_bytes: &[u8],
    filters: &mut LocalReachabilityFilters,
) {
    match data_type_str {
        "collection" => {
            let Ok(view) = remi_things_crdt::extract_collection_doc_view(doc_bytes, uuid) else {
                return;
            };

            let collection_is_live = !view
                .meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false);

            if let Some(active_collections) = filters.active_collections.as_mut() {
                if collection_is_live {
                    active_collections.insert(uuid.to_string());
                } else {
                    active_collections.remove(uuid);
                }
            }

            if !collection_is_live {
                if let Some(active_things) = filters.active_things.as_mut() {
                    for thing in &view.things {
                        active_things.remove(&thing.id);
                    }
                }
                return;
            }

            if let Some(active_things) = filters.active_things.as_mut() {
                for thing in &view.things {
                    if !thing.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false) {
                        active_things.insert(thing.id.clone());
                    } else {
                        active_things.remove(&thing.id);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Sync all dirty v3 CRDT documents with the server in priority order,
/// then pull any server-side documents that are missing locally.
///
/// Phase 1 (push): Dirty documents synced in order: Root → Collections → ThingMarkdown.
/// Phase 1b (receive): Existing local documents whose canonical head differs from the
///   server's current head are re-synced to receive changes made by other devices.
///   When head metadata is unavailable, this falls back to the previous full receive sync.
/// Phase 2 (pull): Discover server-side documents via `list_crdt_document_keys`,
///   download any missing ones via `get_crdt_document_snapshot`, then sync them
///   through the Automerge protocol so both sides share a sync state.
pub async fn sync_v3_documents_with_server(
    sdk: &TriggerSdk,
    client: &mut TriggerClient,
    device_id: &str,
) -> Result<ThingsV3SyncOutput> {
    sync_v3_documents_with_transport_mode(sdk, client, device_id, ThingsSyncMode::Full).await
}

pub async fn sync_v3_documents_with_server_mode(
    sdk: &TriggerSdk,
    client: &mut TriggerClient,
    device_id: &str,
    mode: ThingsSyncMode,
) -> Result<ThingsV3SyncOutput> {
    sync_v3_documents_with_transport_mode(sdk, client, device_id, mode).await
}

pub async fn sync_v3_documents_with_transport<T>(
    sdk: &TriggerSdk,
    client: &mut T,
    device_id: &str,
) -> Result<ThingsV3SyncOutput>
where
    T: CrdtSyncTransport,
{
    sync_v3_documents_with_transport_mode(sdk, client, device_id, ThingsSyncMode::Full).await
}

pub async fn sync_v3_documents_with_transport_mode<T>(
    sdk: &TriggerSdk,
    client: &mut T,
    device_id: &str,
    mode: ThingsSyncMode,
) -> Result<ThingsV3SyncOutput>
where
    T: CrdtSyncTransport,
{
    let total_started_at = Instant::now();
    let mut documents_synced = 0;
    let mut last_sync_at: Option<String> = None;
    let mut effective_mode = mode;
    let mut prefetched_server_keys: Option<Vec<ServerCrdtDocumentKey>> = None;
    let mut bootstrap_pull_ms = 0u64;
    let phase1_push_ms: u64;
    let mut phase1b_receive_ms = 0u64;
    let mut phase2_pull_ms = 0u64;
    let mut list_keys_calls = 0usize;
    let mut snapshot_downloads = 0usize;
    let mut phase1_documents_synced = 0usize;
    let mut phase1b_documents_synced = 0usize;
    let mut phase2_documents_synced = 0usize;
    let mut phase1_rpc_rounds = 0usize;
    let mut phase1b_rpc_rounds = 0usize;
    let mut phase1_batch_calls = 0usize;
    let mut phase1b_batch_calls = 0usize;
    let mut phase1_server_reply_messages = 0usize;
    let mut phase1b_server_reply_messages = 0usize;

    tracing::info!(device_id = device_id, ?mode, "Starting Things v3 sync run");

    // ── Pre-flight: detect first-ever sync ───────────────────────────────
    // If the client has never synced any document, the only local docs are
    // auto-initialised root docs.  Pushing an independently-created root to
    // a server that already holds Device A's root creates an Automerge fork
    // conflict on `collection_uuids` — whichever actor ID sorts higher
    // "wins", which may be the empty list, causing data loss.
    //
    // Prevention: when no document has been synced yet, pull from the server
    // **first**, discarding auto-created local docs that would conflict.

    let dirty_docs = sdk
        .crdt_get_dirty_documents()
        .context("Failed to load dirty CRDT documents")?;

    let bootstrap_state = classify_local_bootstrap_state(sdk, &dirty_docs);
    let never_synced_dirty_keys = never_synced_dirty_keys(&dirty_docs);
    tracing::info!(
        device_id = device_id,
        ?bootstrap_state,
        dirty_doc_count = dirty_docs.len(),
        never_synced_dirty_doc_count = never_synced_dirty_keys.len(),
        "Computed local bootstrap state for Things sync"
    );

    if effective_mode == ThingsSyncMode::Incremental
        && bootstrap_state != LocalBootstrapState::HasSyncedHistory
    {
        tracing::info!(
            device_id = device_id,
            requested_mode = ?mode,
            ?bootstrap_state,
            "Incremental sync upgraded to full sync because bootstrap discovery is required"
        );
        effective_mode = ThingsSyncMode::Full;
    }

    // On true first sync, pull from server before pushing so that locally
    // auto-created docs don't fork the server's canonical root.
    let mut documents_pulled = 0;
    if effective_mode.allows_server_discovery()
        && bootstrap_state != LocalBootstrapState::HasSyncedHistory
    {
        let bootstrap_started_at = Instant::now();
        let had_existing_bootstrap_stash = match sdk.things_bootstrap_has_stash() {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(
                    device_id = device_id,
                    error = %err,
                    "Failed to check for existing bootstrap stash; assuming none"
                );
                false
            }
        };
        let created_bootstrap_stash =
            match sdk.things_bootstrap_stash_local_snapshot_if_needed(device_id) {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!(
                        device_id = device_id,
                        error = %err,
                        "Failed to persist bootstrap stash before destructive bootstrap"
                    );
                    false
                }
            };
        let bootstrap_stash_ready = created_bootstrap_stash || had_existing_bootstrap_stash;
        list_keys_calls += 1;
        let server_keys = client.list_crdt_document_keys().await.unwrap_or_default();
        prefetched_server_keys = Some(server_keys.clone());
        tracing::info!(
            device_id = device_id,
            server_doc_count = server_keys.len(),
            had_existing_bootstrap_stash = had_existing_bootstrap_stash,
            created_bootstrap_stash = created_bootstrap_stash,
            bootstrap_stash_ready = bootstrap_stash_ready,
            "Fetched server keys during bootstrap discovery"
        );

        if !server_keys.is_empty() {
            tracing::info!(
                device_id = device_id,
                server_doc_count = server_keys.len(),
                "First sync detected — pulling server documents before pushing"
            );

            let deleted_local_doc_keys: Vec<String> = never_synced_dirty_keys
                .iter()
                .map(|(uuid, data_type)| format!("{}:{}", uuid, data_type))
                .collect();
            tracing::info!(
                device_id = device_id,
                deleted_local_doc_count = deleted_local_doc_keys.len(),
                deleted_local_doc_keys = ?deleted_local_doc_keys,
                "Deleting never-synced local documents before bootstrap pull"
            );

            if !never_synced_dirty_keys.is_empty() && !bootstrap_stash_ready {
                anyhow::bail!(
                    "Refusing destructive bootstrap without a local stash; {} never-synced dirty CRDT documents would be deleted",
                    never_synced_dirty_keys.len()
                );
            }

            // Delete never-synced auto-created local docs so that
            // `pull_missing_documents` treats them as missing and downloads
            // the server's versions instead.
            for (uuid, data_type) in &never_synced_dirty_keys {
                let _ = sdk.crdt_delete_document(uuid, data_type);
            }

            let pulled =
                pull_missing_documents(sdk, client, device_id, Some(&server_keys), None).await;
            match pulled {
                Ok(output) => {
                    documents_pulled = output.documents_pulled;
                    documents_synced += output.documents_pulled;
                    phase2_documents_synced += output.documents_pulled;
                    snapshot_downloads += output.snapshot_downloads;
                    list_keys_calls += output.list_keys_calls;
                    observe_sync_timestamp(&mut last_sync_at, output.last_sync_at);

                    if bootstrap_stash_ready {
                        sdk.things_bootstrap_replay_stash_onto_current_documents(device_id)
                            .context(
                                "Failed to replay stashed local changes after first-sync bootstrap",
                            )?;
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        device_id = device_id,
                        error = %err,
                        "First-sync pull failed (will continue with push)"
                    );
                }
            }
        }
        bootstrap_pull_ms = bootstrap_started_at.elapsed().as_millis() as u64;
    }

    // ── Phase 1: push dirty local documents ──────────────────────────────
    // Re-fetch dirty list because auto-created docs may have been deleted
    // and pulled docs are saved as clean.

    let dirty_docs = sdk
        .crdt_get_dirty_documents()
        .context("Failed to load dirty CRDT documents")?;

    let mut synced_in_phase1 = std::collections::HashSet::new();
    let phase1_started_at = Instant::now();

    let mut dirty_root_docs = Vec::new();
    let mut dirty_collection_docs = Vec::new();
    let mut dirty_markdown_docs = Vec::new();
    for doc_row in dirty_docs {
        match doc_row.data_type.as_str() {
            "root" => dirty_root_docs.push(doc_row),
            "collection" => dirty_collection_docs.push(doc_row),
            "thing_markdown" => dirty_markdown_docs.push(doc_row),
            _ => {}
        }
    }

    tracing::info!(
        device_id = device_id,
        dirty_root_docs = dirty_root_docs
            .iter()
            .map(|doc| doc.uuid.clone())
            .collect::<Vec<_>>()
            .join(","),
        dirty_collection_docs = dirty_collection_docs
            .iter()
            .map(|doc| doc.uuid.clone())
            .collect::<Vec<_>>()
            .join(","),
        dirty_markdown_docs = dirty_markdown_docs
            .iter()
            .map(|doc| doc.uuid.clone())
            .collect::<Vec<_>>()
            .join(","),
        "Phase 1 dirty document batches prepared"
    );

    for dirty_batch in [dirty_root_docs, dirty_collection_docs, dirty_markdown_docs] {
        let (results, batch_calls) = sync_document_rows_batch(client, device_id, dirty_batch).await;
        phase1_batch_calls += batch_calls;
        for (doc_row, result) in results {
            match result {
                Ok(output) => {
                    synced_in_phase1.insert((doc_row.uuid.clone(), doc_row.data_type.clone()));
                    tracing::info!(
                        device_id = device_id,
                        uuid = doc_row.uuid,
                        data_type = doc_row.data_type,
                        rpc_rounds = output.rpc_rounds,
                        server_reply_messages = output.server_reply_messages,
                        last_sync_at = ?output.last_sync_at,
                        "Phase 1 synced document"
                    );
                    sdk.crdt_save_document(
                        &doc_row.uuid,
                        &doc_row.data_type,
                        &output.doc_bytes,
                        &output.sync_state_bytes,
                        false,
                        output.last_sync_at.as_deref(),
                    )
                    .context("Failed to save synced CRDT document")?;

                    documents_synced += 1;
                    phase1_documents_synced += 1;
                    phase1_rpc_rounds += output.rpc_rounds;
                    phase1_server_reply_messages += output.server_reply_messages;
                    observe_sync_timestamp(&mut last_sync_at, output.last_sync_at);
                }
                Err(err) => {
                    tracing::warn!(
                        device_id = device_id,
                        uuid = doc_row.uuid,
                        data_type = doc_row.data_type,
                        error = %err,
                        "Failed to sync CRDT document, will retry later"
                    );
                }
            }
        }
    }
    phase1_push_ms = phase1_started_at.elapsed().as_millis() as u64;

    tracing::info!(
        device_id = device_id,
        ?effective_mode,
        phase1_documents_synced = documents_synced,
        "Finished phase 1 dirty-document push"
    );

    if !effective_mode.allows_server_discovery() {
        tracing::info!(
            device_id = device_id,
            documents_synced = documents_synced,
            "Skipping server discovery phases because sync is incremental"
        );
        return Ok(ThingsV3SyncOutput {
            documents_synced,
            last_sync_at,
            metrics: ThingsV3SyncMetrics {
                total_elapsed_ms: total_started_at.elapsed().as_millis() as u64,
                bootstrap_pull_ms,
                phase1_push_ms,
                phase1b_receive_ms,
                phase2_pull_ms,
                list_keys_calls,
                snapshot_downloads,
                phase1_documents_synced,
                phase1b_documents_synced,
                phase2_documents_synced,
                phase1_rpc_rounds,
                phase1b_rpc_rounds,
                phase1_batch_calls,
                phase1b_batch_calls,
                phase1_server_reply_messages,
                phase1b_server_reply_messages,
            },
        });
    }

    // ── Phase 1b: receive updates from other devices for existing docs ───
    // Phase 1 only syncs dirty (locally-modified) documents. Clean docs that
    // already exist locally still need a receive path for changes made by
    // OTHER devices. Use the server's canonical head metadata to skip docs
    // that are already converged. If key discovery itself is unavailable,
    // skip the receive/pull phases for this run rather than falling back to
    // a broad receive sync over every local document.
    let server_key_discovery = if let Some(keys) = prefetched_server_keys.take() {
        tracing::info!(
            device_id = device_id,
            server_doc_count = keys.len(),
            "Reusing prefetched server keys for phase 1b/2"
        );
        ServerKeyDiscovery::Available(keys)
    } else {
        match client.list_crdt_document_keys().await {
            Ok(keys) => {
                list_keys_calls += 1;
                tracing::info!(
                    device_id = device_id,
                    server_doc_count = keys.len(),
                    "Fetched server keys for phase 1b/2"
                );
                ServerKeyDiscovery::Available(keys)
            }
            Err(err) => {
                tracing::warn!(
                    device_id = device_id,
                    error = %err,
                    "Failed to list server CRDT keys for phase 1b optimization; skipping receive/pull phases to avoid cold-start sync storms"
                );
                ServerKeyDiscovery::Unavailable
            }
        }
    };
    let server_head_by_key = server_key_discovery
        .keys()
        .map(build_server_head_map)
        .unwrap_or_default();
    let mut reachability = match build_local_reachability_filters(sdk, device_id) {
        Ok(filters) => filters,
        Err(err) => {
            tracing::warn!(
                device_id = device_id,
                error = %err,
                "Failed to build local reachability filters for phase 1b; falling back to broad receive sync"
            );
            LocalReachabilityFilters::default()
        }
    };

    let phase1b_started_at = Instant::now();
    if server_key_discovery.keys().is_some() {
        let mut all_keys = sdk.crdt_list_document_keys().unwrap_or_default();

        // Sort: root first, then collection, then thing_markdown
        all_keys.sort_by_key(|(_, dt)| match dt.as_str() {
            "root" => 0,
            "collection" => 1,
            "thing_markdown" => 2,
            _ => 3,
        });

        let mut receive_root_docs = Vec::new();
        let mut receive_collection_docs = Vec::new();
        let mut receive_markdown_docs = Vec::new();

        for (uuid, data_type_str) in all_keys {
            if synced_in_phase1.contains(&(uuid.clone(), data_type_str.clone())) {
                continue;
            }

            let data_type = match data_type_str.as_str() {
                "root" => CrdtDataType::Root,
                "collection" => CrdtDataType::Collection,
                "thing_markdown" => CrdtDataType::ThingMarkdown,
                _ => continue,
            };

            let doc_row = match sdk.crdt_get_document(&uuid, &data_type_str) {
                Ok(Some(row)) => row,
                _ => continue,
            };

            if !doc_row.dirty
                && !clean_document_should_receive(
                    &uuid,
                    &data_type_str,
                    &doc_row.automerge_doc,
                    &reachability,
                )
            {
                tracing::debug!(
                    device_id = device_id,
                    uuid = uuid,
                    data_type = data_type_str,
                    "Phase 1b: skipping clean unreachable document"
                );
                continue;
            }

            if !doc_row.dirty {
                if let Some(server_head) =
                    server_head_by_key.get(&(uuid.clone(), data_type_str.clone()))
                {
                    if document_is_at_server_head(&doc_row.automerge_doc, server_head) {
                        tracing::debug!(
                            device_id = device_id,
                            uuid = uuid,
                            data_type = data_type_str,
                            "Phase 1b: skipping clean document already at server canonical head"
                        );
                        continue;
                    }
                }
            }

            match data_type {
                CrdtDataType::Root => receive_root_docs.push(doc_row),
                CrdtDataType::Collection => receive_collection_docs.push(doc_row),
                CrdtDataType::ThingMarkdown => receive_markdown_docs.push(doc_row),
            }
        }

        for receive_batch in [
            receive_root_docs,
            receive_collection_docs,
            receive_markdown_docs,
        ] {
            let receive_batch_keys = receive_batch
                .iter()
                .map(|doc| format!("{}:{}", doc.uuid, doc.data_type))
                .collect::<Vec<_>>();
            tracing::info!(
                device_id = device_id,
                receive_doc_count = receive_batch_keys.len(),
                receive_doc_keys = ?receive_batch_keys,
                "Phase 1b receive batch prepared"
            );
            let (results, batch_calls) =
                sync_document_rows_batch(client, device_id, receive_batch).await;
            phase1b_batch_calls += batch_calls;
            for (doc_row, result) in results {
                match result {
                    Ok(output) => {
                        tracing::info!(
                            device_id = device_id,
                            uuid = doc_row.uuid,
                            data_type = doc_row.data_type,
                            rpc_rounds = output.rpc_rounds,
                            server_reply_messages = output.server_reply_messages,
                            last_sync_at = ?output.last_sync_at,
                            "Phase 1b received document updates"
                        );
                        sdk.crdt_save_document(
                            &doc_row.uuid,
                            &doc_row.data_type,
                            &output.doc_bytes,
                            &output.sync_state_bytes,
                            false,
                            output.last_sync_at.as_deref(),
                        )
                        .context("Failed to save synced CRDT document")?;

                        documents_synced += 1;
                        phase1b_documents_synced += 1;
                        phase1b_rpc_rounds += output.rpc_rounds;
                        phase1b_server_reply_messages += output.server_reply_messages;
                        documents_pulled += 1;
                        observe_sync_timestamp(&mut last_sync_at, output.last_sync_at);
                        if doc_row.data_type == "collection" {
                            if let Ok(updated_filters) =
                                build_local_reachability_filters(sdk, device_id)
                            {
                                reachability = updated_filters;
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            device_id = device_id,
                            uuid = doc_row.uuid,
                            data_type = doc_row.data_type,
                            error = %err,
                            "Phase 1b: failed to receive updates for document (non-fatal)"
                        );
                    }
                }
            }
        }
    } else {
        tracing::warn!(
            device_id = device_id,
            "Skipping phase 1b receive sync because server key discovery is unavailable"
        );
    }
    phase1b_receive_ms = phase1b_started_at.elapsed().as_millis() as u64;

    // ── Phase 2: pull any remaining missing server-side documents ────────

    if let Some(server_keys) = server_key_discovery.keys() {
        let phase2_started_at = Instant::now();
        let pulled = pull_missing_documents(
            sdk,
            client,
            device_id,
            Some(server_keys),
            Some(&mut reachability),
        )
        .await;
        match pulled {
            Ok(output) => {
                documents_pulled += output.documents_pulled;
                documents_synced += output.documents_pulled;
                phase2_documents_synced += output.documents_pulled;
                snapshot_downloads += output.snapshot_downloads;
                list_keys_calls += output.list_keys_calls;
                observe_sync_timestamp(&mut last_sync_at, output.last_sync_at);
            }
            Err(err) => {
                tracing::warn!(
                    device_id = device_id,
                    error = %err,
                    "Failed to pull missing server documents (non-fatal)"
                );
            }
        }
        phase2_pull_ms = phase2_started_at.elapsed().as_millis() as u64;
    } else {
        tracing::warn!(
            device_id = device_id,
            "Skipping phase 2 pull because server key discovery is unavailable"
        );
    }

    // ── Notify clients if new documents were pulled ──────────────────────
    // Emit a SnapshotReplaced event so the UI refreshes with the newly
    // downloaded collections/things without requiring a manual reload.
    if documents_pulled > 0 {
        if let Err(err) = sdk.emit_snapshot_replace(device_id) {
            tracing::warn!(
                device_id = device_id,
                error = %err,
                "Failed to emit SnapshotReplaced after pull (non-fatal)"
            );
        }
    }

    tracing::info!(
        device_id = device_id,
        ?effective_mode,
        documents_synced = documents_synced,
        last_sync_at = ?last_sync_at,
        "Completed Things v3 sync run"
    );

    Ok(ThingsV3SyncOutput {
        documents_synced,
        last_sync_at,
        metrics: ThingsV3SyncMetrics {
            total_elapsed_ms: total_started_at.elapsed().as_millis() as u64,
            bootstrap_pull_ms,
            phase1_push_ms,
            phase1b_receive_ms,
            phase2_pull_ms,
            list_keys_calls,
            snapshot_downloads,
            phase1_documents_synced,
            phase1b_documents_synced,
            phase2_documents_synced,
            phase1_rpc_rounds,
            phase1b_rpc_rounds,
            phase1_batch_calls,
            phase1b_batch_calls,
            phase1_server_reply_messages,
            phase1b_server_reply_messages,
        },
    })
}

/// Discover server-side CRDT documents and download any that are missing locally.
///
/// Downloaded snapshots are saved directly with an initial sync state instead of
/// immediately performing a follow-up Automerge handshake. That extra handshake
/// was generating large numbers of no-op round trips during cold-start bootstrap
/// because the snapshot already contains the canonical server document bytes.
///
/// Returns pulled-document counts plus timing/work metrics useful for profiling.
async fn pull_missing_documents<T>(
    sdk: &TriggerSdk,
    client: &mut T,
    device_id: &str,
    prefetched_server_keys: Option<&[ServerCrdtDocumentKey]>,
    mut reachability: Option<&mut LocalReachabilityFilters>,
) -> Result<PullMissingDocumentsOutput>
where
    T: CrdtSyncTransport,
{
    // Get a set of all local document keys for fast lookup
    let local_keys: std::collections::HashSet<(String, String)> = sdk
        .crdt_list_document_keys()
        .context("Failed to list local CRDT document keys")?
        .into_iter()
        .collect();

    let mut owned_reachability = None;
    let reachability = match reachability.as_deref_mut() {
        Some(filters) => filters,
        None => {
            owned_reachability = Some(match build_local_reachability_filters(sdk, device_id) {
                Ok(filters) => filters,
                Err(err) => {
                    tracing::warn!(
                        device_id = device_id,
                        error = %err,
                        "Failed to build local reachability filters for phase 2; falling back to broad pull"
                    );
                    LocalReachabilityFilters::default()
                }
            });
            owned_reachability
                .as_mut()
                .expect("owned reachability should be initialized")
        }
    };

    // Ask the server for its full list of document keys unless the caller
    // already fetched them for phase 1b.
    let fetched_server_keys;
    let server_keys: &[ServerCrdtDocumentKey] = if let Some(keys) = prefetched_server_keys {
        keys
    } else {
        fetched_server_keys = client
            .list_crdt_document_keys()
            .await
            .context("Failed to list server CRDT document keys")?;
        &fetched_server_keys
    };

    // Determine which server documents are missing locally
    let mut missing: Vec<(String, i32)> = Vec::new();
    for key in server_keys {
        let uuid = &key.document_uuid;
        let proto_dt = key.data_type;
        let dt_str = proto_data_type_to_str(proto_dt);
        if dt_str.is_empty() || local_keys.contains(&(uuid.clone(), dt_str.to_string())) {
            continue;
        }

        missing.push((uuid.clone(), proto_dt));
    }

    if missing.is_empty() {
        tracing::debug!(device_id = device_id, "No missing server documents to pull");
        return Ok(PullMissingDocumentsOutput {
            documents_pulled: 0,
            last_sync_at: None,
            snapshot_downloads: 0,
            list_keys_calls: usize::from(prefetched_server_keys.is_none()),
        });
    }

    tracing::info!(
        device_id = device_id,
        count = missing.len(),
        "Pulling missing server documents"
    );

    let mut documents_pulled = 0;
    let mut last_sync_at: Option<String> = None;
    let mut local_snapshot_downloads = 0usize;

    // Sort: Root first, then Collection, then ThingMarkdown (matches push order)
    missing.sort_by_key(|(_, dt)| *dt);

    let batch_documents: Vec<(String, i32)> = missing
        .iter()
        .filter_map(|(uuid, proto_dt)| {
            let dt_str = proto_data_type_to_str(*proto_dt);
            if should_pull_missing_document(uuid, dt_str, reachability) {
                Some((uuid.clone(), *proto_dt))
            } else {
                tracing::debug!(
                    uuid = %uuid,
                    data_type = dt_str,
                    "Skipping pull for unreachable document"
                );
                None
            }
        })
        .collect();

    if batch_documents.is_empty() {
        return Ok(PullMissingDocumentsOutput {
            documents_pulled: 0,
            last_sync_at: None,
            snapshot_downloads: 0,
            list_keys_calls: usize::from(prefetched_server_keys.is_none()),
        });
    }

    match client
        .get_crdt_document_snapshots(device_id.to_string(), batch_documents, true)
        .await
    {
        Ok(snapshots) => {
            for (uuid, proto_dt, doc_bytes, sync_at) in snapshots {
                local_snapshot_downloads += 1;
                let dt_str = proto_data_type_to_str(proto_dt);

                if doc_bytes.is_empty() {
                    tracing::debug!(
                        uuid = uuid,
                        data_type = dt_str,
                        "Server returned empty doc, skipping"
                    );
                    continue;
                }

                let snapshot_sync_at = optional_sync_timestamp(sync_at);
                sdk.crdt_save_document(
                    &uuid,
                    dt_str,
                    &doc_bytes,
                    &[],
                    false,
                    snapshot_sync_at.as_deref(),
                )
                .context("Failed to save pulled CRDT document snapshot")?;

                update_reachability_from_downloaded_doc(&uuid, dt_str, &doc_bytes, reachability);
                if dt_str == "collection" {
                    if let Ok(updated_filters) = build_local_reachability_filters(sdk, device_id) {
                        *reachability = updated_filters;
                    }
                }

                documents_pulled += 1;
                observe_sync_timestamp(&mut last_sync_at, snapshot_sync_at);
            }
        }
        Err(err) => {
            tracing::warn!(
                device_id = device_id,
                error = %err,
                "Batch snapshot download failed, falling back to per-document pulls"
            );

            for (uuid, proto_dt) in &missing {
                let dt_str = proto_data_type_to_str(*proto_dt);
                if !should_pull_missing_document(uuid, dt_str, reachability) {
                    continue;
                }

                match client
                    .get_crdt_document_snapshot(
                        device_id.to_string(),
                        uuid.clone(),
                        *proto_dt,
                        true,
                    )
                    .await
                {
                    Ok((doc_bytes, sync_at)) => {
                        local_snapshot_downloads += 1;
                        if doc_bytes.is_empty() {
                            tracing::debug!(
                                uuid = uuid,
                                data_type = dt_str,
                                "Server returned empty doc, skipping"
                            );
                            continue;
                        }

                        let snapshot_sync_at = optional_sync_timestamp(sync_at);
                        sdk.crdt_save_document(
                            uuid,
                            dt_str,
                            &doc_bytes,
                            &[],
                            false,
                            snapshot_sync_at.as_deref(),
                        )
                        .context("Failed to save pulled CRDT document snapshot")?;

                        update_reachability_from_downloaded_doc(
                            uuid,
                            dt_str,
                            &doc_bytes,
                            reachability,
                        );
                        if dt_str == "collection" {
                            if let Ok(updated_filters) =
                                build_local_reachability_filters(sdk, device_id)
                            {
                                *reachability = updated_filters;
                            }
                        }

                        documents_pulled += 1;
                        observe_sync_timestamp(&mut last_sync_at, snapshot_sync_at);
                    }
                    Err(err) => {
                        tracing::warn!(
                            uuid = uuid,
                            data_type = dt_str,
                            error = %err,
                            "Failed to download CRDT document snapshot, skipping"
                        );
                    }
                }
            }
        }
    }

    Ok(PullMissingDocumentsOutput {
        documents_pulled,
        last_sync_at,
        snapshot_downloads: local_snapshot_downloads,
        list_keys_calls: usize::from(prefetched_server_keys.is_none()),
    })
}

/// Convert proto data_type integer to storage string.
fn proto_data_type_to_str(proto_dt: i32) -> &'static str {
    match proto_dt {
        1 => "root",
        2 => "collection",
        3 => "thing_markdown",
        _ => "",
    }
}

/// Sync a single v3 CRDT document with the server
async fn sync_single_v3_document<T>(
    client: &mut T,
    device_id: &str,
    uuid: &str,
    data_type: &CrdtDataType,
    doc_bytes: Vec<u8>,
    sync_state_bytes: Vec<u8>,
) -> Result<ThingsSyncOutput>
where
    T: CrdtSyncTransport,
{
    let mut session = crdt_sync::AutomergeSyncSession::new_with_device_id(
        &doc_bytes,
        &sync_state_bytes,
        device_id,
    )
    .context("Failed to init CRDT document sync session")?;

    const MAX_ROUNDS: usize = 20;
    const MAX_STALL_ROUNDS: usize = 3;

    let mut last_sync_at = None;
    let mut server_msgs: Vec<Vec<u8>> = Vec::new();
    let mut prev_outgoing: Vec<u8> = Vec::new();
    let mut prev_server_msgs: Vec<Vec<u8>> = Vec::new();
    let mut stall_rounds: usize = 0;
    let mut rpc_rounds: usize = 0;
    let mut server_reply_messages: usize = 0;

    let proto_data_type = data_type_to_proto(data_type);

    for round in 0..MAX_ROUNDS {
        if !server_msgs.is_empty() {
            session
                .apply_server_messages(&server_msgs)
                .context("Failed to apply server messages for CRDT document")?;
            server_msgs.clear();
        }

        let outgoing = session.generate_client_message().unwrap_or_default();
        let outgoing_for_compare = outgoing.clone();

        if outgoing.is_empty() {
            tracing::debug!(
                device_id = device_id,
                uuid = uuid,
                data_type = data_type.as_str(),
                round = round + 1,
                "sync_single_v3_document: converged with no outgoing message"
            );
            break;
        }

        // Use the v3 sync endpoint with document key
        let (next_server_msgs, last) = client
            .sync_crdt_document(
                device_id.to_string(),
                uuid.to_string(),
                proto_data_type,
                outgoing,
            )
            .await
            .context("Failed to sync CRDT document with server")?;
        rpc_rounds += 1;
        server_reply_messages += next_server_msgs.len();

        last_sync_at = optional_sync_timestamp(last);

        if outgoing_for_compare == prev_outgoing && next_server_msgs == prev_server_msgs {
            stall_rounds += 1;
        } else {
            stall_rounds = 0;
        }

        prev_outgoing = outgoing_for_compare;
        prev_server_msgs = next_server_msgs.clone();
        server_msgs = next_server_msgs;

        let reply_bytes: usize = server_msgs.iter().map(|msg| msg.len()).sum();
        tracing::debug!(
            device_id = device_id,
            uuid = uuid,
            data_type = data_type.as_str(),
            round = round + 1,
            outgoing_bytes = prev_outgoing.len(),
            reply_count = server_msgs.len(),
            reply_bytes = reply_bytes,
            stall_rounds = stall_rounds,
            "sync_single_v3_document: round complete"
        );

        if stall_rounds >= MAX_STALL_ROUNDS {
            tracing::warn!(
                device_id = device_id,
                uuid = uuid,
                data_type = data_type.as_str(),
                round = round + 1,
                "sync_single_v3_document: breaking after repeated identical handshake rounds"
            );
            session
                .apply_server_messages(&server_msgs)
                .context("Failed to apply stalled server messages")?;
            server_msgs.clear();
            break;
        }
    }

    if !server_msgs.is_empty() {
        session
            .apply_server_messages(&server_msgs)
            .context("Failed to apply final server messages")?;
    }

    Ok(ThingsSyncOutput {
        doc_bytes: session.doc_bytes(),
        sync_state_bytes: session.sync_state_bytes(),
        last_sync_at,
        rpc_rounds,
        server_reply_messages,
    })
}

struct BatchSyncSession {
    uuid: String,
    data_type: CrdtDataType,
    session: crdt_sync::AutomergeSyncSession,
    pending_server_messages: Vec<Vec<u8>>,
    prev_outgoing: Vec<u8>,
    prev_server_messages: Vec<Vec<u8>>,
    stall_rounds: usize,
    last_sync_at: Option<String>,
    rpc_rounds: usize,
    server_reply_messages: usize,
    finished: bool,
}

struct BatchSyncRunOutput {
    outputs: Vec<ThingsSyncOutput>,
    batch_calls: usize,
}

async fn sync_v3_document_batch<T>(
    client: &mut T,
    device_id: &str,
    documents: Vec<(String, CrdtDataType, Vec<u8>, Vec<u8>)>,
) -> Result<BatchSyncRunOutput>
where
    T: CrdtSyncTransport,
{
    if documents.is_empty() {
        return Ok(BatchSyncRunOutput {
            outputs: Vec::new(),
            batch_calls: 0,
        });
    }

    const MAX_ROUNDS: usize = 20;
    const MAX_STALL_ROUNDS: usize = 3;

    let mut sessions = Vec::with_capacity(documents.len());
    for (uuid, data_type, doc_bytes, sync_state_bytes) in documents {
        sessions.push(BatchSyncSession {
            uuid,
            data_type,
            session: crdt_sync::AutomergeSyncSession::new_with_device_id(
                &doc_bytes,
                &sync_state_bytes,
                device_id,
            )
            .context("Failed to init CRDT document sync session")?,
            pending_server_messages: Vec::new(),
            prev_outgoing: Vec::new(),
            prev_server_messages: Vec::new(),
            stall_rounds: 0,
            last_sync_at: None,
            rpc_rounds: 0,
            server_reply_messages: 0,
            finished: false,
        });
    }

    let mut batch_calls = 0usize;
    for round in 0..MAX_ROUNDS {
        let mut batch_requests = Vec::new();
        let mut request_indices = Vec::new();

        for (index, state) in sessions.iter_mut().enumerate() {
            if state.finished {
                continue;
            }

            if !state.pending_server_messages.is_empty() {
                state
                    .session
                    .apply_server_messages(&state.pending_server_messages)
                    .context("Failed to apply server messages for CRDT document batch")?;
                state.pending_server_messages.clear();
            }

            let outgoing = state.session.generate_client_message().unwrap_or_default();
            if outgoing.is_empty() {
                tracing::debug!(
                    device_id = device_id,
                    uuid = state.uuid,
                    data_type = state.data_type.as_str(),
                    round = round + 1,
                    "sync_v3_document_batch: document converged with no outgoing message"
                );
                state.finished = true;
                continue;
            }

            request_indices.push((index, outgoing.clone()));
            batch_requests.push((
                state.uuid.clone(),
                data_type_to_proto(&state.data_type),
                outgoing,
            ));
        }

        if batch_requests.is_empty() {
            break;
        }

        let responses = client
            .sync_crdt_documents(device_id.to_string(), batch_requests)
            .await
            .context("Failed to sync CRDT document batch with server")?;
        batch_calls += 1;
        let mut responses_by_key = responses
            .into_iter()
            .map(|(document_uuid, data_type, sync_messages, last_sync_at)| {
                ((document_uuid, data_type), (sync_messages, last_sync_at))
            })
            .collect::<std::collections::HashMap<_, _>>();

        for (index, outgoing_for_compare) in request_indices {
            let state = &mut sessions[index];
            let response_key = (state.uuid.clone(), data_type_to_proto(&state.data_type));
            let (next_server_messages, last_sync_at) =
                responses_by_key.remove(&response_key).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Batch CRDT sync response missing document {} ({})",
                        state.uuid,
                        state.data_type.as_str()
                    )
                })?;

            state.rpc_rounds += 1;
            state.server_reply_messages += next_server_messages.len();
            state.last_sync_at = optional_sync_timestamp(last_sync_at);

            if outgoing_for_compare == state.prev_outgoing
                && next_server_messages == state.prev_server_messages
            {
                state.stall_rounds += 1;
            } else {
                state.stall_rounds = 0;
            }

            state.prev_outgoing = outgoing_for_compare;
            state.prev_server_messages = next_server_messages.clone();
            state.pending_server_messages = next_server_messages;

            let reply_bytes: usize = state
                .pending_server_messages
                .iter()
                .map(|msg| msg.len())
                .sum();
            tracing::debug!(
                device_id = device_id,
                uuid = state.uuid,
                data_type = state.data_type.as_str(),
                round = round + 1,
                outgoing_bytes = state.prev_outgoing.len(),
                reply_count = state.pending_server_messages.len(),
                reply_bytes = reply_bytes,
                stall_rounds = state.stall_rounds,
                "sync_v3_document_batch: round complete"
            );

            if state.stall_rounds >= MAX_STALL_ROUNDS {
                tracing::warn!(
                    device_id = device_id,
                    uuid = state.uuid,
                    data_type = state.data_type.as_str(),
                    round = round + 1,
                    "sync_v3_document_batch: breaking after repeated identical handshake rounds"
                );
                state
                    .session
                    .apply_server_messages(&state.pending_server_messages)
                    .context("Failed to apply stalled server messages in batch sync")?;
                state.pending_server_messages.clear();
                state.finished = true;
            }
        }

        if !responses_by_key.is_empty() {
            return Err(anyhow::anyhow!(
                "Batch CRDT sync returned {} unexpected document responses",
                responses_by_key.len()
            ));
        }
    }

    let mut outputs = Vec::with_capacity(sessions.len());
    for mut state in sessions {
        if !state.pending_server_messages.is_empty() {
            state
                .session
                .apply_server_messages(&state.pending_server_messages)
                .context("Failed to apply final server messages for batched CRDT sync")?;
        }

        outputs.push(ThingsSyncOutput {
            doc_bytes: state.session.doc_bytes(),
            sync_state_bytes: state.session.sync_state_bytes(),
            last_sync_at: state.last_sync_at,
            rpc_rounds: state.rpc_rounds,
            server_reply_messages: state.server_reply_messages,
        });
    }

    Ok(BatchSyncRunOutput {
        outputs,
        batch_calls,
    })
}

async fn sync_document_rows_batch<T>(
    client: &mut T,
    device_id: &str,
    doc_rows: Vec<crate::types::CrdtDocumentRow>,
) -> (
    Vec<(crate::types::CrdtDocumentRow, Result<ThingsSyncOutput>)>,
    usize,
)
where
    T: CrdtSyncTransport,
{
    if doc_rows.is_empty() {
        return (Vec::new(), 0);
    }

    let batch_inputs = match doc_rows
        .iter()
        .map(|doc_row| {
            Ok((
                doc_row.uuid.clone(),
                parse_row_data_type(&doc_row.data_type)?,
                doc_row.automerge_doc.clone(),
                doc_row.sync_state.clone(),
            ))
        })
        .collect::<Result<Vec<_>>>()
    {
        Ok(inputs) => inputs,
        Err(err) => {
            return (
                doc_rows
                    .into_iter()
                    .map(|doc_row| {
                        let message =
                            format!("Failed to prepare CRDT document for batch sync: {err}");
                        (doc_row, Err(anyhow::anyhow!(message)))
                    })
                    .collect(),
                0,
            );
        }
    };

    match sync_v3_document_batch(client, device_id, batch_inputs).await {
        Ok(run_output) => (
            doc_rows
                .into_iter()
                .zip(run_output.outputs.into_iter().map(Ok))
                .collect(),
            run_output.batch_calls,
        ),
        Err(err) => {
            tracing::warn!(
                device_id = device_id,
                error = %err,
                document_count = doc_rows.len(),
                "Batch CRDT sync failed; falling back to per-document sync"
            );

            let mut results = Vec::with_capacity(doc_rows.len());
            for doc_row in doc_rows {
                let result = match parse_row_data_type(&doc_row.data_type) {
                    Ok(data_type) => {
                        sync_single_v3_document(
                            client,
                            device_id,
                            &doc_row.uuid,
                            &data_type,
                            doc_row.automerge_doc.clone(),
                            doc_row.sync_state.clone(),
                        )
                        .await
                    }
                    Err(err) => Err(err),
                };
                results.push((doc_row, result));
            }
            (results, 0)
        }
    }
}

/// Load all v3 CRDT documents from storage into a ThingsDocumentSet
pub fn load_document_set_from_storage(
    sdk: &TriggerSdk,
    device_id: &str,
) -> Result<ThingsDocumentSet> {
    DocumentPersistence::new(sdk).load_document_set(device_id)
}

/// Save a ThingsDocumentSet back to storage
pub fn save_document_set_to_storage(sdk: &TriggerSdk, doc_set: &ThingsDocumentSet) -> Result<()> {
    DocumentPersistence::new(sdk).save_document_set(doc_set)
}

/// Save only dirty documents from a ThingsDocumentSet to storage
pub fn save_dirty_documents_to_storage(
    sdk: &TriggerSdk,
    doc_set: &ThingsDocumentSet,
) -> Result<usize> {
    DocumentPersistence::new(sdk).save_dirty_documents(doc_set)
}

#[cfg(test)]
mod tests_v3 {
    use super::*;
    use crate::things_crdt::DocumentKey;
    use async_trait::async_trait;
    use automerge::ROOT;
    use automerge::transaction::Transactable;
    use std::path::PathBuf;
    use tempfile::Builder;

    struct MockSyncTransport {
        list_calls: usize,
        snapshot_calls: usize,
        batch_snapshot_calls: usize,
        batch_sync_calls: usize,
        sync_calls: usize,
        server_keys: Vec<ServerCrdtDocumentKey>,
        snapshots: std::collections::HashMap<(String, i32), (Vec<u8>, String)>,
    }

    #[async_trait]
    impl CrdtSyncTransport for MockSyncTransport {
        async fn sync_crdt_document(
            &mut self,
            _device_id: String,
            _document_uuid: String,
            _data_type: i32,
            _sync_message: Vec<u8>,
        ) -> Result<(Vec<Vec<u8>>, String)> {
            self.sync_calls += 1;
            Ok((Vec::new(), String::new()))
        }

        async fn sync_crdt_documents(
            &mut self,
            _device_id: String,
            documents: Vec<(String, i32, Vec<u8>)>,
        ) -> Result<Vec<(String, i32, Vec<Vec<u8>>, String)>> {
            self.batch_sync_calls += 1;
            self.sync_calls += documents.len();
            Ok(documents
                .into_iter()
                .map(|(document_uuid, data_type, _sync_message)| {
                    (document_uuid, data_type, Vec::new(), String::new())
                })
                .collect())
        }

        async fn get_crdt_document_snapshot(
            &mut self,
            _device_id: String,
            document_uuid: String,
            data_type: i32,
            _reset_sync_state: bool,
        ) -> Result<(Vec<u8>, String)> {
            self.snapshot_calls += 1;
            Ok(self
                .snapshots
                .get(&(document_uuid, data_type))
                .cloned()
                .unwrap_or_else(|| (Vec::new(), String::new())))
        }

        async fn get_crdt_document_snapshots(
            &mut self,
            _device_id: String,
            documents: Vec<(String, i32)>,
            _reset_sync_state: bool,
        ) -> Result<Vec<(String, i32, Vec<u8>, String)>> {
            self.batch_snapshot_calls += 1;
            Ok(documents
                .into_iter()
                .map(|(document_uuid, data_type)| {
                    let (automerge_doc, last_sync_at) = self
                        .snapshots
                        .get(&(document_uuid.clone(), data_type))
                        .cloned()
                        .unwrap_or_else(|| (Vec::new(), String::new()));
                    (document_uuid, data_type, automerge_doc, last_sync_at)
                })
                .collect())
        }

        async fn list_crdt_document_keys(&mut self) -> Result<Vec<ServerCrdtDocumentKey>> {
            self.list_calls += 1;
            Ok(self.server_keys.clone())
        }
    }

    fn test_sdk() -> TriggerSdk {
        let dir = Builder::new()
            .prefix("remi-things-sync-test-")
            .tempdir()
            .expect("tempdir")
            .keep();
        let db_path: PathBuf = dir.join("sdk.sqlite3");
        TriggerSdk::initialize(&db_path).expect("sdk init")
    }

    fn mutated_root_doc(device_id: &str) -> Vec<u8> {
        let doc_bytes = remi_things_crdt::Schema::init_root_doc(device_id).expect("init root doc");
        let mut doc = automerge::AutoCommit::load(&doc_bytes).expect("load root doc");
        doc.put(ROOT, "_sync_test_marker", "changed")
            .expect("mutate root doc");
        doc.save()
    }

    fn advanced_sync_state_for(device_id: &str) -> Vec<u8> {
        let _ = device_id;
        vec![1, 2, 3]
    }

    fn seed_dirty_root_document(sdk: &TriggerSdk, doc: &[u8], sync_state: Vec<u8>) {
        sdk.crdt_save_document("root", "root", doc, &sync_state, true, None)
            .expect("save dirty root doc");
    }

    fn seed_dirty_collection_document(
        sdk: &TriggerSdk,
        uuid: &str,
        device_id: &str,
        sync_state: Vec<u8>,
    ) {
        let doc = remi_things_crdt::Schema::init_collection_doc(device_id, uuid)
            .expect("init collection doc");
        sdk.crdt_save_document(uuid, "collection", &doc, &sync_state, true, None)
            .expect("save dirty collection doc");
    }

    fn test_doc_row(
        sync_state: Vec<u8>,
        last_sync_at: Option<&str>,
    ) -> crate::types::CrdtDocumentRow {
        crate::types::CrdtDocumentRow {
            uuid: "doc-1".to_string(),
            data_type: "root".to_string(),
            automerge_doc: Vec::new(),
            sync_state,
            dirty: true,
            last_sync_at: last_sync_at.map(str::to_string),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_document_key_data_type_str() {
        let root = DocumentKey::root();
        assert_eq!(root.data_type_str(), "root");

        let coll = DocumentKey::collection("coll-1");
        assert_eq!(coll.data_type_str(), "collection");

        let md = DocumentKey::thing_markdown("thing-1");
        assert_eq!(md.data_type_str(), "thing_markdown");
    }

    #[test]
    fn document_head_match_detects_identical_single_head_doc() {
        let doc = remi_things_crdt::Schema::init_root_doc("device-a").unwrap();
        let head = local_canonical_head(&doc).unwrap();

        assert!(document_is_at_server_head(&doc, &head));
    }

    #[test]
    fn document_head_match_requires_non_empty_server_head() {
        let doc = remi_things_crdt::Schema::init_root_doc("device-a").unwrap();

        assert!(!document_is_at_server_head(&doc, &[]));
    }

    #[test]
    fn sync_history_ignores_last_sync_at_when_sync_state_is_initial() {
        let initial = crate::crdt_sync::init_sync_state();

        assert!(!has_sync_history(&test_doc_row(
            Vec::new(),
            Some("2026-03-21T00:00:00Z")
        )));
        assert!(!has_sync_history(&test_doc_row(
            initial,
            Some("2026-03-21T00:00:00Z")
        )));
    }

    #[test]
    fn sync_history_detects_advanced_sync_state_without_last_sync_at() {
        let advanced_state = advanced_sync_state_for("device-a");

        assert!(has_sync_history(&test_doc_row(advanced_state, None)));
    }

    #[test]
    fn never_synced_dirty_keys_follows_sync_state_history() {
        let advanced_state = advanced_sync_state_for("device-a");

        let unsynced = crate::types::CrdtDocumentRow {
            uuid: "unsynced".to_string(),
            ..test_doc_row(
                crate::crdt_sync::init_sync_state(),
                Some("2026-03-21T00:00:00Z"),
            )
        };
        let synced = crate::types::CrdtDocumentRow {
            uuid: "synced".to_string(),
            ..test_doc_row(advanced_state, None)
        };

        let keys = never_synced_dirty_keys(&[unsynced, synced]);
        assert_eq!(keys, vec![("unsynced".to_string(), "root".to_string())]);
    }

    #[tokio::test]
    async fn incremental_mode_skips_server_key_discovery_when_sync_history_exists() {
        let sdk = test_sdk();
        let device_id = "device-a";
        let advanced_state = advanced_sync_state_for(device_id);
        let synced_doc = mutated_root_doc(device_id);
        seed_dirty_root_document(&sdk, &synced_doc, advanced_state);

        let mut transport = MockSyncTransport {
            list_calls: 0,
            snapshot_calls: 0,
            batch_snapshot_calls: 0,
            batch_sync_calls: 0,
            sync_calls: 0,
            server_keys: Vec::new(),
            snapshots: std::collections::HashMap::new(),
        };

        let output = sync_v3_documents_with_transport_mode(
            &sdk,
            &mut transport,
            device_id,
            ThingsSyncMode::Incremental,
        )
        .await
        .expect("incremental sync succeeds");

        assert_eq!(transport.list_calls, 0);
        assert_eq!(transport.snapshot_calls, 0);
        assert_eq!(transport.sync_calls, 1);
        assert_eq!(transport.batch_sync_calls, 1);
        assert_eq!(output.documents_synced, 1);
    }

    #[tokio::test]
    async fn incremental_mode_upgrades_to_full_when_bootstrap_discovery_is_required() {
        let sdk = test_sdk();
        seed_dirty_root_document(
            &sdk,
            &mutated_root_doc("device-a"),
            crate::crdt_sync::init_sync_state(),
        );

        let mut transport = MockSyncTransport {
            list_calls: 0,
            snapshot_calls: 0,
            batch_snapshot_calls: 0,
            batch_sync_calls: 0,
            sync_calls: 0,
            server_keys: Vec::new(),
            snapshots: std::collections::HashMap::new(),
        };

        let output = sync_v3_documents_with_transport_mode(
            &sdk,
            &mut transport,
            "device-a",
            ThingsSyncMode::Incremental,
        )
        .await
        .expect("bootstrap sync succeeds");

        assert_eq!(transport.list_calls, 1);
        assert!(transport.sync_calls >= 1);
        assert!(transport.batch_sync_calls >= 1);
        assert!(output.documents_synced >= 1);
    }

    #[tokio::test]
    async fn pull_missing_documents_saves_snapshots_without_followup_sync_roundtrips() {
        let sdk = test_sdk();
        let device_id = "device-a";
        let root_doc =
            remi_things_crdt::Schema::init_root_doc("server-device").expect("init root doc");
        let root_head = local_canonical_head(&root_doc).expect("root head");

        let mut transport = MockSyncTransport {
            list_calls: 0,
            snapshot_calls: 0,
            batch_snapshot_calls: 0,
            batch_sync_calls: 0,
            sync_calls: 0,
            server_keys: vec![ServerCrdtDocumentKey {
                document_uuid: "root".to_string(),
                data_type: 1,
                canonical_head: root_head,
            }],
            snapshots: std::collections::HashMap::from([(
                ("root".to_string(), 1),
                (root_doc.clone(), "2026-03-25T00:00:00Z".to_string()),
            )]),
        };

        let prefetched_keys = transport.server_keys.clone();

        let output = pull_missing_documents(
            &sdk,
            &mut transport,
            device_id,
            Some(&prefetched_keys),
            None,
        )
        .await
        .expect("pull_missing_documents succeeds");

        assert_eq!(transport.list_calls, 0);
        assert_eq!(transport.snapshot_calls, 0);
        assert_eq!(transport.batch_snapshot_calls, 1);
        assert_eq!(transport.sync_calls, 0);
        assert_eq!(output.documents_pulled, 1);
        assert_eq!(output.last_sync_at.as_deref(), Some("2026-03-25T00:00:00Z"));
        assert_eq!(output.snapshot_downloads, 1);

        let saved = sdk
            .crdt_get_document("root", "root")
            .expect("load root")
            .expect("root exists after pull");
        assert_eq!(saved.automerge_doc, root_doc);
        assert!(!saved.dirty);
    }

    #[tokio::test]
    async fn phase1_batches_same_priority_documents_into_one_transport_call() {
        let sdk = test_sdk();
        let device_id = "device-a";
        seed_dirty_collection_document(
            &sdk,
            "collection-a",
            device_id,
            advanced_sync_state_for(device_id),
        );
        seed_dirty_collection_document(
            &sdk,
            "collection-b",
            device_id,
            advanced_sync_state_for(device_id),
        );

        let mut transport = MockSyncTransport {
            list_calls: 0,
            snapshot_calls: 0,
            batch_snapshot_calls: 0,
            batch_sync_calls: 0,
            sync_calls: 0,
            server_keys: Vec::new(),
            snapshots: std::collections::HashMap::new(),
        };

        let output = sync_v3_documents_with_transport_mode(
            &sdk,
            &mut transport,
            device_id,
            ThingsSyncMode::Full,
        )
        .await
        .expect("batched phase1 sync succeeds");

        assert_eq!(transport.batch_sync_calls, 1);
        assert_eq!(transport.sync_calls, 2);
        assert_eq!(output.documents_synced, 2);
    }
}
