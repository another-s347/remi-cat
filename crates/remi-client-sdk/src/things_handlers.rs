//! Built-in external tool handlers for Things operations and Trigger publishing.

use crate::TriggerSdk;
use crate::chat_types::{RichHandlerResult, ToolImagePart};
use crate::external_tool_handler::ExternalToolHandler;
use crate::external_tools::ExternalToolExecutor;
use crate::remi_uri::{RemiUri, RemiUriLocation, mime_from_extension};
use crate::things_crdt::{ThingCollectionUpsert, ThingDatatype, ThingUpsert, ThingsSnapshot};
use crate::types::{TriggerRegistration, TriggerRule};
use async_trait::async_trait;
use serde::Deserializer;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::sync::Arc;

// ══════════════════════════════════════════════════════════════════════════════�?
// Things Interrupt Payloads
// ══════════════════════════════════════════════════════════════════════════════�?

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingCollectionInfo {
    pub uuid: String,
    pub title: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingInfo {
    #[serde(default, deserialize_with = "deserialize_string_or_default")]
    pub uuid: String,
    #[serde(default, deserialize_with = "deserialize_string_or_default")]
    pub title: String,
    #[serde(default, deserialize_with = "deserialize_string_or_default")]
    pub datatype: String,
    #[serde(default, deserialize_with = "deserialize_string_or_default")]
    pub data_json: String,
    #[serde(default)]
    pub parent_uuid: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_default")]
    pub collection_uuid: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThingsThingEnvelope {
    pub thing: ThingInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThingsCollectionEnvelope {
    pub collection: ThingCollectionInfo,
}

fn parse_thing_info(data: &JsonValue) -> Result<ThingInfo, String> {
    if let Some(thing_value) = data.get("thing").filter(|value| !value.is_null()) {
        if let Ok(info) = serde_json::from_value::<ThingInfo>(thing_value.clone()) {
            return Ok(info);
        }
    }

    let direct_err = match serde_json::from_value::<ThingInfo>(data.clone()) {
        Ok(info) => return Ok(info),
        Err(err) => err,
    };

    serde_json::from_value::<ThingsThingEnvelope>(data.clone())
        .map(|envelope| envelope.thing)
        .map_err(|envelope_err| {
            format!(
                "Failed to parse thing payload: {direct_err}; envelope parse failed: {envelope_err}"
            )
        })
}

fn deserialize_string_or_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

fn normalize_optional_string(s: Option<String>) -> Option<String> {
    match s {
        None => None,
        Some(v) => {
            let t = v.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        }
    }
}

fn normalize_string(v: &str) -> Option<String> {
    let t = v.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn normalize_required_uuid(value: &str, field: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(format!("{field} is required"))
    } else {
        Ok(trimmed.to_string())
    }
}

fn load_things_snapshot(
    sdk: &TriggerSdk,
    device_id: &str,
    include_things: bool,
) -> Result<ThingsSnapshot, String> {
    let snapshot = sdk
        .things_list_snapshot_with_options(
            device_id,
            include_things,
            crate::things_crdt::SnapshotOptions {
                include_content: false,
            },
        )
        .map_err(|e| format!("Failed to read local things snapshot: {e}"))?;

    Ok(ThingsSnapshot {
        collections: snapshot.collections,
        things: snapshot.things,
    })
}

/// Payload for things_thing_content_edit - matches agent's interrupt data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingContentEdit {
    pub uuid: String,
    pub operation: String,
    #[serde(default)]
    pub new_title: Option<String>,
    #[serde(default)]
    pub new_content: Option<String>,
    #[serde(default)]
    pub old_str: Option<String>,
    #[serde(default)]
    pub new_str: Option<String>,
    #[serde(default)]
    pub line_number: Option<i64>,
    #[serde(default)]
    pub insert_text: Option<String>,
    #[serde(default)]
    pub append_text: Option<String>,
}

/// Payload for things_thing_removed - matches agent's interrupt data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingRemoveInfo {
    pub uuid: String,
    #[serde(default)]
    pub collection_uuid: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Payload for things_thing_moved - matches agent's interrupt data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingMoveInfo {
    pub uuid: String,
    pub to_collection_uuid: String,
    #[serde(default)]
    pub to_parent_uuid: Option<String>,
}

/// Payload for events_retrieve_request - matches agent's interrupt data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsRetrieveRequest {
    pub start_time: String,
    pub end_time: String,
}

/// Payload for events_abstract_request - matches agent's interrupt data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsAbstractRequest {
    #[serde(default)]
    pub top_n: Option<JsonValue>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VirtualFsTreeRequest {
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VirtualFsLsRequest {
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualFsCatRequest {
    pub path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VirtualFsEditRequest {
    pub path: String,
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default)]
    pub value: Option<JsonValue>,
    #[serde(default)]
    pub old_str: Option<String>,
    #[serde(default)]
    pub new_str: Option<String>,
    #[serde(default)]
    pub line_number: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualFsDeleteRequest {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualFsMoveRequest {
    pub from_path: String,
    pub to_path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VirtualFsCreateRequest {
    pub parent_path: String,
    #[serde(default)]
    pub type_name: String,
    #[serde(default)]
    pub action_uuid: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub source_uri: Option<String>,
    #[serde(default)]
    pub bind_path: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
}

// ══════════════════════════════════════════════════════════════════════════════�?
// Handler Implementations
// ══════════════════════════════════════════════════════════════════════════════�?

/// Handler for things_collection_added
pub struct CollectionAddedHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl CollectionAddedHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for CollectionAddedHandler {
    async fn handle(&self, interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        tracing::debug!(
            interrupt_id = %interrupt_id,
            payload = %payload,
            "[CollectionAddedHandler] Received interrupt"
        );
        let data = payload.clone();
        tracing::debug!(
            extracted_data = %data,
            "[CollectionAddedHandler] Extracted data from payload"
        );
        let info: ThingCollectionInfo = if data.get("collection").is_some() {
            let env: ThingsCollectionEnvelope = serde_json::from_value(data)
                .map_err(|e| format!("Failed to parse collection envelope: {}", e))?;
            env.collection
        } else {
            serde_json::from_value(data)
                .map_err(|e| format!("Failed to parse collection: {}", e))?
        };

        let collection_uuid = normalize_required_uuid(&info.uuid, "uuid")?;

        self.sdk
            .things_upsert_collection(
                &self.device_id,
                ThingCollectionUpsert {
                    uuid: collection_uuid.clone(),
                    title: info.title.clone(),
                    trigger_uuid: None,
                    created_at: normalize_optional_string(info.created_at),
                    updated_at: normalize_optional_string(info.updated_at),
                },
            )
            .map_err(|e| format!("Failed to upsert collection: {}", e))?;

        tracing::info!(
            uuid = %collection_uuid,
            title = %info.title,
            "[CollectionAddedHandler] Collection added via interrupt"
        );

        Ok(json!({
            "confirmed": true,
            "uuid": collection_uuid,
        }))
    }
}

/// Handler for things_collection_edited
pub struct CollectionEditedHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl CollectionEditedHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for CollectionEditedHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let info: ThingCollectionInfo = if data.get("collection").is_some() {
            let env: ThingsCollectionEnvelope = serde_json::from_value(data)
                .map_err(|e| format!("Failed to parse collection envelope: {}", e))?;
            env.collection
        } else {
            serde_json::from_value(data)
                .map_err(|e| format!("Failed to parse collection: {}", e))?
        };

        let collection_uuid = normalize_required_uuid(&info.uuid, "uuid")?;

        self.sdk
            .things_upsert_collection(
                &self.device_id,
                ThingCollectionUpsert {
                    uuid: collection_uuid.clone(),
                    title: info.title.clone(),
                    trigger_uuid: None,
                    created_at: normalize_optional_string(info.created_at),
                    updated_at: normalize_optional_string(info.updated_at),
                },
            )
            .map_err(|e| format!("Failed to upsert collection: {}", e))?;

        tracing::info!(uuid = %collection_uuid, title = %info.title, "Collection edited via interrupt");

        Ok(json!({
            "confirmed": true,        }))
    }
}

/// Handler for things_collection_removed
pub struct CollectionRemovedHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl CollectionRemovedHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for CollectionRemovedHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let uuid = data
            .get("uuid")
            .and_then(|v| v.as_str())
            .ok_or("Missing uuid in collection_removed")?;

        self.sdk
            .things_delete_collection(&self.device_id, uuid)
            .map_err(|e| format!("Failed to delete collection: {}", e))?;

        tracing::info!(uuid, "Collection removed via interrupt");

        Ok(json!({
            "confirmed": true,        }))
    }
}

/// Handler for things_thing_added
pub struct ThingAddedHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl ThingAddedHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for ThingAddedHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let info = parse_thing_info(&data)?;

        let thing_uuid = normalize_required_uuid(&info.uuid, "uuid")?;
        let collection_uuid = normalize_required_uuid(&info.collection_uuid, "collection_uuid")?;
        let datatype = normalize_string(&info.datatype).unwrap_or_else(|| "markdown".to_string());

        let snapshot = load_things_snapshot(&self.sdk, &self.device_id, false)?;
        if snapshot
            .collections
            .iter()
            .find(|c| c.uuid == collection_uuid)
            .is_none()
        {
            return Err(format!("Collection not found: {collection_uuid}"));
        }

        let data_value: Option<JsonValue> =
            normalize_string(&info.data_json).and_then(|s| serde_json::from_str(&s).ok());

        self.sdk
            .things_upsert_thing(
                &self.device_id,
                ThingUpsert {
                    uuid: thing_uuid.clone(),
                    title: info.title.clone(),
                    datatype: ThingDatatype::from_str(&datatype),
                    data: data_value,
                    collection_uuid: collection_uuid.clone(),
                    trigger_uuid: None,
                    parent_uuid: normalize_optional_string(info.parent_uuid),
                    created_at: normalize_optional_string(info.created_at),
                    updated_at: normalize_optional_string(info.updated_at),
                },
            )
            .map_err(|e| format!("Failed to upsert thing: {}", e))?;

        tracing::info!(uuid = %thing_uuid, title = %info.title, "Thing added via interrupt");

        Ok(json!({
            "confirmed": true,
            "uuid": thing_uuid,
            "collection_uuid": collection_uuid,
        }))
    }
}

/// Handler for things_thing_edited
pub struct ThingEditedHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl ThingEditedHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for ThingEditedHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let info = parse_thing_info(&data)?;

        let thing_uuid = normalize_required_uuid(&info.uuid, "uuid")?;
        let collection_uuid = normalize_required_uuid(&info.collection_uuid, "collection_uuid")?;
        let datatype = normalize_string(&info.datatype).unwrap_or_else(|| "markdown".to_string());

        let data_value: Option<JsonValue> =
            normalize_string(&info.data_json).and_then(|s| serde_json::from_str(&s).ok());

        self.sdk
            .things_upsert_thing(
                &self.device_id,
                ThingUpsert {
                    uuid: thing_uuid.clone(),
                    title: info.title.clone(),
                    datatype: ThingDatatype::from_str(&datatype),
                    data: data_value,
                    collection_uuid: collection_uuid.clone(),
                    trigger_uuid: None,
                    parent_uuid: normalize_optional_string(info.parent_uuid),
                    created_at: normalize_optional_string(info.created_at),
                    updated_at: normalize_optional_string(info.updated_at),
                },
            )
            .map_err(|e| format!("Failed to upsert thing: {}", e))?;

        tracing::info!(uuid = %thing_uuid, title = %info.title, "Thing edited via interrupt");

        Ok(json!({
            "confirmed": true,        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn parse_thing_info_prefers_embedded_thing_object() {
        let info = parse_thing_info(&json!({
            "type": "things_thing_added",
            "thing": {
                "uuid": "thing-1",
                "title": "hello",
                "datatype": null,
                "data_json": null,
                "collection_uuid": "collection-1"
            }
        }))
        .expect("embedded thing object should deserialize");

        assert_eq!(info.uuid, "thing-1");
        assert_eq!(info.collection_uuid, "collection-1");
        assert_eq!(info.datatype, "");
        assert_eq!(info.data_json, "");
    }

    #[test]
    fn parse_thing_info_falls_back_when_thing_key_is_null() {
        let info = parse_thing_info(&json!({
            "type": "things_thing_added",
            "thing": null,
            "uuid": "thing-2",
            "title": "hello",
            "datatype": null,
            "data_json": null,
            "collection_uuid": "collection-2"
        }))
        .expect("direct payload should deserialize when thing is null");

        assert_eq!(info.uuid, "thing-2");
        assert_eq!(info.collection_uuid, "collection-2");
        assert_eq!(info.datatype, "");
        assert_eq!(info.data_json, "");
    }

    #[test]
    fn thing_envelope_allows_nullable_required_strings() {
        let envelope: ThingsThingEnvelope = serde_json::from_value(json!({
            "thing": {
                "uuid": null,
                "title": "hello",
                "datatype": null,
                "data_json": null,
                "parent_uuid": null,
                "collection_uuid": "collection-1",
                "created_at": null,
                "updated_at": null
            }
        }))
        .expect("envelope should deserialize");

        assert_eq!(envelope.thing.uuid, "");
        assert_eq!(envelope.thing.datatype, "");
        assert_eq!(envelope.thing.data_json, "");
        assert_eq!(envelope.thing.collection_uuid, "collection-1");
        assert_eq!(envelope.thing.parent_uuid, None);
    }

    #[tokio::test]
    async fn virtual_fs_cat_handler_returns_image_parts_for_image_entries() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let image_path = dir.path().join("sample.png");
        std::fs::write(&image_path, [137u8, 80, 78, 71]).expect("write image");

        let sdk = Arc::new(TriggerSdk::initialize(&db_path).expect("sdk"));
        let device_id = "device-test";
        sdk.things_upsert_collection(
            device_id,
            ThingCollectionUpsert {
                uuid: "c1".to_string(),
                title: "Inbox".to_string(),
                trigger_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("collection");
        sdk.things_upsert_thing(
            device_id,
            ThingUpsert {
                uuid: "t1".to_string(),
                title: "Photo".to_string(),
                datatype: ThingDatatype::Markdown,
                data: Some(json!({"markdown": ""})),
                collection_uuid: "c1".to_string(),
                trigger_uuid: None,
                parent_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("thing");
        let image_uri =
            RemiUri::from_local_file(&image_path.to_string_lossy(), "image/png", device_id)
                .to_uri_string();
        sdk.things_add_content_entry(
            device_id,
            "t1",
            crate::things_crdt::ContentEntry {
                id: "entry-1".to_string(),
                title: Some("Image".to_string()),
                order: 0.0,
                payload: crate::things_crdt::ContentEntryPayload::Image(
                    crate::things_crdt::ImageField::new(image_uri),
                ),
            },
        )
        .expect("entry");

        let handler = VirtualFsCatHandler::new(sdk, device_id.to_string());
        let result = handler
            .handle_rich(
                "call-1",
                &json!({
                    "type": "virtual_fs_cat_request",
                    "path": "/collection/c1/things/t1/entries.0"
                }),
            )
            .await
            .expect("cat rich result");

        match result {
            RichHandlerResult::Image(part) => {
                assert_eq!(part.media_type, "image/png");
                assert_eq!(part.data, vec![137u8, 80, 78, 71]);
            }
            RichHandlerResult::Json(other) => panic!("expected image result, got {other}"),
        }
    }

    #[tokio::test]
    async fn virtual_fs_edit_handler_accepts_object_value_for_trigger_rule() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");

        let sdk = Arc::new(TriggerSdk::initialize(&db_path).expect("sdk"));
        let device_id = "device-test";
        sdk.register_trigger(TriggerRegistration {
            trigger_uuid: "t1".to_string(),
            name: "Watch VSCode".to_string(),
            version: "1.0".to_string(),
            precondition: Vec::new(),
            condition: Vec::new(),
            action_uuid: None,
            action_args: json!({}),
        })
        .expect("create trigger");

        let handler = VirtualFsEditHandler::new(sdk.clone(), device_id.to_string());
        let result = handler
            .handle(
                "call-1",
                &json!({
                    "type": "virtual_fs_edit_request",
                    "path": "/trigger/t1/rule.json",
                    "value": {
                        "precondition": [{ "description": "watch app", "rule": "event('App')" }],
                        "condition": [{ "description": "open vscode", "rule": "event_exists_with_message(1, '', 'VSCode')" }]
                    }
                }),
            )
            .await
            .expect("edit trigger rule");

        assert_eq!(result.get("ok").and_then(JsonValue::as_bool), Some(true));
        assert_eq!(
            result.get("path").and_then(JsonValue::as_str),
            Some("/trigger/t1/rule.json")
        );
    }

    #[tokio::test]
    async fn events_retrieve_handler_includes_available_time_range_for_empty_window() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = Arc::new(TriggerSdk::initialize(&db_path).expect("sdk init"));

        let timestamp = chrono::DateTime::parse_from_rfc3339("2026-04-02T01:15:00+00:00")
            .expect("timestamp")
            .with_timezone(&chrono::Utc);
        sdk.record_event(crate::types::EventPayload {
            event_type: "DesktopAppFocus".to_string(),
            timestamp,
            metadata: json!({ "window_title": "VSCode" }),
        })
        .expect("record event");

        let handler = EventsRetrieveRequestHandler::new(sdk);
        let result = handler
            .handle(
                "call-1",
                &json!({
                    "type": "events_retrieve_request",
                    "start_time": "2026-01-20T00:00:00+08:00",
                    "end_time": "2026-01-23T23:59:59+08:00"
                }),
            )
            .await
            .expect("retrieve events");

        assert_eq!(
            result
                .get("events")
                .and_then(JsonValue::as_array)
                .map(Vec::len),
            Some(0)
        );
        assert_eq!(
            result
                .get("available_time_range")
                .and_then(|value| value.get("start_time"))
                .and_then(JsonValue::as_str),
            Some("2026-04-02T01:15:00+00:00")
        );
        assert_eq!(
            result
                .get("available_time_range")
                .and_then(|value| value.get("end_time"))
                .and_then(JsonValue::as_str),
            Some("2026-04-02T01:15:00+00:00")
        );
        assert!(
            result
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .contains("Recorded events are available between 2026-04-02T01:15:00+00:00 and 2026-04-02T01:15:00+00:00"),
            "message should point the caller at the existing event time bounds"
        );
    }

    #[tokio::test]
    async fn virtual_fs_create_handler_returns_trigger_next_edit_hint() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");

        let sdk = Arc::new(TriggerSdk::initialize(&db_path).expect("sdk"));
        let device_id = "device-test";
        let handler = VirtualFsCreateHandler::new(sdk, device_id.to_string());

        let result = handler
            .handle(
                "call-1",
                &json!({
                    "type": "virtual_fs_create_request",
                    "parent_path": "/trigger",
                    "type_name": "trigger",
                    "title": "Watch VSCode",
                    "uuid": "trigger-1"
                }),
            )
            .await
            .expect("create trigger");

        assert_eq!(
            result.get("path").and_then(JsonValue::as_str),
            Some("/trigger/trigger-1")
        );
        assert_eq!(
            result.get("next_edit_path").and_then(JsonValue::as_str),
            Some("/trigger/trigger-1/rule.json")
        );
        assert!(
            result
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .contains("/trigger/trigger-1/rule.json")
        );
    }
}

/// Handler for things_thing_removed
pub struct ThingRemovedHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl ThingRemovedHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for ThingRemovedHandler {
    async fn handle(&self, interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        tracing::info!(interrupt_id = %interrupt_id, payload = %payload, "[ThingRemovedHandler] Received interrupt");

        let data = payload.clone();
        tracing::info!(data = %data, "[ThingRemovedHandler] Extracted data");

        let info: ThingRemoveInfo = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse thing_removed: {}", e))?;
        tracing::info!(uuid = %info.uuid, collection_uuid = ?info.collection_uuid, "[ThingRemovedHandler] Parsed info");

        // collection_uuid is ignored by v2 delete implementation, pass empty string if not provided
        let collection_uuid = info.collection_uuid.as_deref().unwrap_or("");

        tracing::info!(device_id = %self.device_id, collection_uuid = %collection_uuid, uuid = %info.uuid, "[ThingRemovedHandler] Calling things_delete_thing");
        self.sdk
            .things_delete_thing(&self.device_id, collection_uuid, &info.uuid)
            .map_err(|e| format!("Failed to delete thing: {}", e))?;

        tracing::info!(uuid = %info.uuid, "[ThingRemovedHandler] Thing removed successfully");

        Ok(json!({
            "confirmed": true,        }))
    }
}

/// Handler for things_thing_content_edit
pub struct ThingContentEditHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl ThingContentEditHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for ThingContentEditHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let edit: ThingContentEdit = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse content edit: {}", e))?;

        let result = self
            .sdk
            .things_edit_content(
                &self.device_id,
                &edit.uuid,
                &edit.operation,
                edit.new_title.as_deref(),
                edit.new_content.as_deref(),
                edit.old_str.as_deref(),
                edit.new_str.as_deref(),
                edit.line_number.map(|n| n as usize),
                edit.insert_text.as_deref(),
                edit.append_text.as_deref(),
            )
            .map_err(|e| format!("Failed to edit content: {}", e))?;

        tracing::info!(uuid = %edit.uuid, operation = %edit.operation, "Thing content edited via interrupt");

        // Return the edit result for resume_value
        let result_json: JsonValue =
            serde_json::from_str(&result).unwrap_or_else(|_| json!({"result": result}));

        Ok(json!({
            "confirmed": true,            "result": result_json,
        }))
    }
}

/// Handler for things_thing_moved
pub struct ThingMovedHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl ThingMovedHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for ThingMovedHandler {
    async fn handle(&self, interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        tracing::info!(interrupt_id = %interrupt_id, payload = %payload, "[ThingMovedHandler] Received interrupt");

        let data = payload.clone();
        tracing::info!(data = %data, "[ThingMovedHandler] Extracted data");

        let info: ThingMoveInfo = match serde_json::from_value(data.clone()) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, data = %data, "[ThingMovedHandler] Failed to parse payload");
                return Ok(json!({
                    "confirmed": false,                    "error": format!("Failed to parse things_thing_moved: {e}"),
                }));
            }
        };
        tracing::info!(uuid = %info.uuid, to_collection_uuid = %info.to_collection_uuid, to_parent_uuid = ?info.to_parent_uuid, "[ThingMovedHandler] Parsed info");

        let to_collection_uuid = info.to_collection_uuid.trim();
        if to_collection_uuid.is_empty() {
            return Ok(json!({
                "confirmed": false,                "error": "to_collection_uuid is required",
            }));
        }

        let snapshot = match self.sdk.things_list_snapshot(&self.device_id) {
            Ok(v) => v,
            Err(e) => {
                return Ok(json!({
                    "confirmed": false,                    "uuid": info.uuid,
                    "error": format!("Failed to read local things snapshot: {e}"),
                }));
            }
        };

        let existing = match snapshot.things.iter().find(|t| t.uuid == info.uuid) {
            Some(t) => t,
            None => {
                tracing::warn!(uuid = %info.uuid, "[ThingMovedHandler] Thing not found in local snapshot");
                return Ok(json!({
                    "confirmed": false,                    "uuid": info.uuid,
                    "error": "Thing not found in local snapshot",
                }));
            }
        };
        tracing::info!(existing_uuid = %existing.uuid, existing_title = %existing.title, "[ThingMovedHandler] Found existing thing");

        let parent_uuid = info
            .to_parent_uuid
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Perform move as an upsert that preserves title/datatype, and only changes collection/parent.
        // We intentionally omit `data` so content is not rewritten.
        tracing::info!(thing_uuid = %existing.uuid, "[ThingMovedHandler] Calling things_upsert_thing");

        if let Err(e) = self.sdk.things_upsert_thing(
            &self.device_id,
            ThingUpsert {
                uuid: existing.uuid.clone(),
                title: existing.title.clone(),
                datatype: existing.datatype.clone(),
                data: None,
                collection_uuid: to_collection_uuid.to_string(),
                trigger_uuid: existing.trigger_uuid.clone(),
                parent_uuid: Some(parent_uuid.clone().unwrap_or_default()),
                created_at: None,
                updated_at: None,
            },
        ) {
            tracing::error!(error = %e, "[ThingMovedHandler] Failed to upsert");
            return Ok(json!({
                "confirmed": false,                "uuid": info.uuid,
                "error": format!("Failed to apply move via upsert: {e}"),
            }));
        }

        tracing::info!(uuid = %existing.uuid, "[ThingMovedHandler] Thing moved successfully");

        Ok(json!({
            "confirmed": true,            "uuid": existing.uuid,
            "to_collection_uuid": to_collection_uuid,
            "to_parent_uuid": parent_uuid,
        }))
    }
}

/// Handler for events_retrieve_request
pub struct EventsRetrieveRequestHandler {
    sdk: Arc<TriggerSdk>,
}

impl EventsRetrieveRequestHandler {
    pub fn new(sdk: Arc<TriggerSdk>) -> Self {
        Self { sdk }
    }
}

#[async_trait]
impl ExternalToolHandler for EventsRetrieveRequestHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: EventsRetrieveRequest = match serde_json::from_value(data) {
            Ok(v) => v,
            Err(e) => {
                return Ok(
                    json!({                    "error": format!("Failed to parse events_retrieve_request: {e}"),
                        "events": [],
                    }),
                );
            }
        };

        match self
            .sdk
            .events_list_between_json(&req.start_time, &req.end_time)
        {
            Ok(events_json) => {
                let events: JsonValue =
                    serde_json::from_str(&events_json).unwrap_or(JsonValue::String(events_json));
                if let Some(items) = events.as_array() {
                    if items.is_empty() {
                        if let Ok(Some((start, end))) = self.sdk.event_time_range() {
                            let start_time = start.to_rfc3339();
                            let end_time = end.to_rfc3339();
                            return Ok(json!({
                                "events": [],
                                "message": format!(
                                    "No events found in the requested time window. Recorded events are available between {start_time} and {end_time}."
                                ),
                                "requested_time_range": {
                                    "start_time": req.start_time,
                                    "end_time": req.end_time,
                                },
                                "available_time_range": {
                                    "start_time": start_time,
                                    "end_time": end_time,
                                },
                            }));
                        }
                    }
                }

                Ok(json!({
                    "events": events,
                }))
            }
            Err(e) => Ok(
                json!({                "error": format!("Failed to list events: {e}"),
                    "events": [],
                }),
            ),
        }
    }
}

/// Handler for events_abstract_request
pub struct EventsAbstractRequestHandler {
    sdk: Arc<TriggerSdk>,
}

impl EventsAbstractRequestHandler {
    pub fn new(sdk: Arc<TriggerSdk>) -> Self {
        Self { sdk }
    }
}

#[async_trait]
impl ExternalToolHandler for EventsAbstractRequestHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: EventsAbstractRequest =
            serde_json::from_value(data).unwrap_or(EventsAbstractRequest { top_n: None });

        let top_n: u32 = match req.top_n {
            Some(JsonValue::Number(n)) => n.as_u64().unwrap_or(3) as u32,
            Some(JsonValue::String(s)) => s.trim().parse::<u32>().unwrap_or(3),
            Some(JsonValue::Bool(b)) => {
                if b {
                    1
                } else {
                    3
                }
            }
            Some(_) => 3,
            None => 3,
        };

        match self.sdk.events_abstract_json(top_n) {
            Ok(summary_json) => {
                let summary: JsonValue =
                    serde_json::from_str(&summary_json).unwrap_or(JsonValue::String(summary_json));
                // Return the summary directly - interrupt_id is added by the registry
                // as the key in the resume map, not as a field inside the value.
                Ok(summary)
            }
            Err(e) => Ok(
                json!({                "error": format!("Failed to abstract events: {e}"),
                    "hours": [],
                    "top_n": top_n,
                }),
            ),
        }
    }
}

/// Handler for things_list_snapshot_request - returns current things state
pub struct ThingsListSnapshotHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThingsListSnapshotRequest {
    #[serde(default)]
    pub entity_type: Option<String>,
    #[serde(default)]
    pub include_content: Option<bool>,
}

impl ThingsListSnapshotHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for ThingsListSnapshotHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: ThingsListSnapshotRequest = serde_json::from_value(data).unwrap_or_default();

        let entity_type = req
            .entity_type
            .unwrap_or_else(|| "all".to_string())
            .trim()
            .to_lowercase();
        let include_things = !matches!(entity_type.as_str(), "collection" | "collections");
        let include_content = req.include_content.unwrap_or(false);

        let snapshot = self
            .sdk
            .things_list_snapshot_with_options(
                &self.device_id,
                include_things,
                crate::things_crdt::SnapshotOptions { include_content },
            )
            .map_err(|e| format!("Failed to get snapshot: {}", e))?;

        tracing::info!("Things list snapshot provided via interrupt");

        Ok(json!({            "things_list_snapshot": snapshot,
        }))
    }
}

/// Payload for things_get_thing_markdown_request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingsGetThingMarkdownRequest {
    pub uuid: String,
}

/// Handler for things_get_thing_markdown_request - returns markdown for a single thing
pub struct ThingsGetThingMarkdownHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl ThingsGetThingMarkdownHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for ThingsGetThingMarkdownHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: ThingsGetThingMarkdownRequest = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse things_get_thing_markdown_request: {e}"))?;

        let markdown = self
            .sdk
            .things_get_thing_markdown(&self.device_id, &req.uuid)
            .map_err(|e| format!("Failed to get thing markdown: {e}"))?
            .unwrap_or_default();

        let content_entries = self
            .sdk
            .things_get_content_entries(&self.device_id, &req.uuid)
            .map(|items| serde_json::to_value(items).unwrap_or(JsonValue::Array(vec![])))
            .unwrap_or_else(|_| JsonValue::Array(vec![]));

        Ok(json!({
            "thing_markdown": {
                "uuid": req.uuid,
                "markdown": markdown,
                "content_entries": content_entries,
            }
        }))
    }
}

pub struct VirtualFsTreeHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl VirtualFsTreeHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for VirtualFsTreeHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: VirtualFsTreeRequest = serde_json::from_value(data).unwrap_or_default();

        self.sdk
            .tree_virtual_path(&self.device_id, req.path.as_deref())
            .map(JsonValue::String)
            .map_err(handler_error_json)
    }
}

pub struct VirtualFsLsHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl VirtualFsLsHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for VirtualFsLsHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: VirtualFsLsRequest = serde_json::from_value(data).unwrap_or_default();

        self.sdk
            .ls_virtual_path(&self.device_id, req.path.as_deref())
            .map(JsonValue::String)
            .map_err(handler_error_json)
    }
}

pub struct VirtualFsCatHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl VirtualFsCatHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for VirtualFsCatHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: VirtualFsCatRequest = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse virtual_fs_cat_request: {e}"))?;

        self.sdk
            .read_virtual_path(&self.device_id, &req.path)
            .and_then(|result| serde_json::to_value(result).map_err(anyhow::Error::from))
            .map_err(handler_error_json)
    }

    async fn handle_rich(
        &self,
        _interrupt_id: &str,
        payload: &JsonValue,
    ) -> Result<RichHandlerResult, String> {
        let data = payload.clone();
        let req: VirtualFsCatRequest = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse virtual_fs_cat_request: {e}"))?;

        match self
            .sdk
            .cat_virtual_path(&self.device_id, &req.path)
            .map_err(handler_error_json)?
        {
            crate::runtime::VirtualFsCatResult::Text(result) => serde_json::to_value(result)
                .map(RichHandlerResult::Json)
                .map_err(|e| format!("Failed to serialize cat result: {e}")),
            crate::runtime::VirtualFsCatResult::Image { uri, .. } => {
                load_image_part(&uri).await.map(RichHandlerResult::Image)
            }
        }
    }
}

pub struct VirtualFsEditHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl VirtualFsEditHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for VirtualFsEditHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: VirtualFsEditRequest = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse virtual_fs_edit_request: {e}"))?;

        self.sdk
            .edit_virtual_path(
                &self.device_id,
                &req.path,
                req.operation.as_deref().unwrap_or("overwrite"),
                req.value.as_ref(),
                req.old_str.as_deref(),
                req.new_str.as_deref(),
                req.line_number.map(|value| value.max(0) as usize),
            )
            .map_err(handler_error_json)
    }
}

pub struct VirtualFsDeleteHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl VirtualFsDeleteHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for VirtualFsDeleteHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: VirtualFsDeleteRequest = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse virtual_fs_delete_request: {e}"))?;

        self.sdk
            .delete_virtual_path(&self.device_id, &req.path)
            .map_err(handler_error_json)
    }
}

pub struct VirtualFsMoveHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl VirtualFsMoveHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for VirtualFsMoveHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: VirtualFsMoveRequest = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse virtual_fs_move_request: {e}"))?;

        self.sdk
            .move_virtual_path(&self.device_id, &req.from_path, &req.to_path)
            .map_err(handler_error_json)
    }
}

pub struct VirtualFsCreateHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl VirtualFsCreateHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for VirtualFsCreateHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: VirtualFsCreateRequest = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse virtual_fs_create_request: {e}"))?;

        self.sdk
            .create_virtual_path(
                &self.device_id,
                &req.parent_path,
                &req.type_name,
                req.action_uuid.as_deref(),
                req.title.as_deref(),
                req.content.as_deref(),
                req.source_uri.as_deref(),
                req.bind_path.as_deref(),
                req.uuid.as_deref(),
            )
            .map_err(handler_error_json)
    }
}

// ══════════════════════════════════════════════════════════════════════════════�?
// Trigger Rule Published Handler
// ══════════════════════════════════════════════════════════════════════════════�?

/// Payload for trigger_rule_published - matches agent's interrupt data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRulePublished {
    pub trigger_uuid: String,
    pub name: String,
    /// The trigger config JSON containing name, precondition, condition arrays
    pub rule_config_json: JsonValue,
    #[serde(default)]
    pub user_request: Option<String>,
    #[serde(default)]
    pub event_analysis: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    pub bind_uuid: String,
    pub bind_type: String, // "thing" or "collection"
    #[serde(default)]
    pub version: Option<JsonValue>,
}

/// Handler for trigger_rule_published - installs trigger locally and binds to entity
pub struct TriggerRulePublishedHandler {
    sdk: Arc<TriggerSdk>,
    #[allow(dead_code)]
    device_id: String,
}

impl TriggerRulePublishedHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for TriggerRulePublishedHandler {
    async fn handle(&self, interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        tracing::debug!(
            interrupt_id = %interrupt_id,
            payload = %payload,
            "[TriggerRulePublishedHandler] Received external tool call"
        );
        let data = payload.clone();
        let info: TriggerRulePublished = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse trigger_rule_published: {}", e))?;

        let trigger_uuid = normalize_required_uuid(&info.trigger_uuid, "trigger_uuid")?;
        let bind_uuid = normalize_required_uuid(&info.bind_uuid, "bind_uuid")?;
        let bind_type = info.bind_type.trim().to_lowercase();

        tracing::info!(
            trigger_uuid = %trigger_uuid,
            bind_uuid = %bind_uuid,
            bind_type = %bind_type,
            name = %info.name,
            "[TriggerRulePublishedHandler] Parsed trigger rule info"
        );

        // Parse the rule_config_json to extract precondition and condition arrays.
        // The backend may send this as either a JSON object or a JSON string.
        let rule_config: JsonValue = match info.rule_config_json {
            JsonValue::String(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
            other => other,
        };

        // Extract precondition array
        let precondition: Vec<TriggerRule> = rule_config
            .get("precondition")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // Extract condition array
        let condition: Vec<TriggerRule> = rule_config
            .get("condition")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // Build TriggerRegistration
        let version = match info.version {
            Some(JsonValue::String(s)) if !s.trim().is_empty() => s,
            Some(JsonValue::Number(n)) => n.to_string(),
            Some(JsonValue::Bool(b)) => b.to_string(),
            Some(v) => v.to_string(),
            None => "1.0".to_string(),
        };

        let registration = TriggerRegistration {
            trigger_uuid: trigger_uuid.clone(),
            name: info.name.clone(),
            version,
            precondition,
            condition,
            action_uuid: None,
            action_args: json!({}),
        };

        // Bind first; if binding fails we must not register or install the trigger.
        let entity_type = match bind_type.as_str() {
            "thing" => "thing",
            "collection" => "collection",
            other => {
                return Err(format!("Invalid bind_type: {}", other));
            }
        };

        let existing_trigger_uuid = match entity_type {
            "collection" => {
                let snapshot = load_things_snapshot(&self.sdk, &self.device_id, false)?;
                let collection_uuids: Vec<&str> = snapshot
                    .collections
                    .iter()
                    .map(|c| c.uuid.as_str())
                    .collect();
                tracing::info!(
                    bind_uuid = %bind_uuid,
                    existing_collections = ?collection_uuids,
                    "[TriggerRulePublishedHandler] Looking for collection in snapshot"
                );
                let collection = snapshot
                    .collections
                    .iter()
                    .find(|c| c.uuid == bind_uuid)
                    .ok_or_else(|| format!("Collection not found: {bind_uuid}"))?;
                collection.trigger_uuid.clone()
            }
            "thing" => {
                let snapshot = load_things_snapshot(&self.sdk, &self.device_id, true)?;
                let thing_uuids: Vec<&str> =
                    snapshot.things.iter().map(|t| t.uuid.as_str()).collect();
                tracing::info!(
                    bind_uuid = %bind_uuid,
                    existing_things = ?thing_uuids,
                    "[TriggerRulePublishedHandler] Looking for thing in snapshot"
                );
                let thing = snapshot
                    .things
                    .iter()
                    .find(|t| t.uuid == bind_uuid)
                    .ok_or_else(|| format!("Thing not found: {bind_uuid}"))?;
                thing.trigger_uuid.clone()
            }
            _ => unreachable!(),
        };

        // Fallback: if the CRDT snapshot has no trigger_uuid for this entity, check the
        // trigger_bindings table. This handles edge cases where the CRDT may be out of sync
        // (e.g., after migration, or the binding was set through a non-CRDT path). Without
        // this, the old trigger registration would survive in the `triggers` table and keep
        // firing after the entity is rebound.
        let existing_trigger_uuid = if existing_trigger_uuid.is_none() {
            match self.sdk.get_trigger_for_entity(entity_type, &bind_uuid) {
                Ok(found) => {
                    if found.is_some() {
                        tracing::info!(
                            bind_uuid = %bind_uuid,
                            entity_type = %entity_type,
                            old_trigger_uuid = ?found,
                            "[TriggerRulePublishedHandler] CRDT had no trigger_uuid; \
                             found stale binding in trigger_bindings table"
                        );
                    }
                    found
                }
                Err(e) => {
                    tracing::warn!(
                        "[TriggerRulePublishedHandler] Failed to query trigger_bindings \
                         for entity {} {}: {}",
                        entity_type,
                        bind_uuid,
                        e
                    );
                    None
                }
            }
        } else {
            existing_trigger_uuid
        };

        let bind_result = match entity_type {
            "collection" => self.sdk.things_set_collection_trigger_uuid(
                &self.device_id,
                &bind_uuid,
                Some(&trigger_uuid),
            ),
            "thing" => self.sdk.things_set_thing_trigger_uuid(
                &self.device_id,
                &bind_uuid,
                Some(&trigger_uuid),
            ),
            _ => unreachable!(),
        };

        if let Err(e) = bind_result {
            tracing::warn!(
                "Failed to bind trigger to {} {}: {}",
                entity_type,
                bind_uuid,
                e
            );
            return Err(format!(
                "Failed to bind trigger to {} {}: {}",
                entity_type, bind_uuid, e
            ));
        }

        // Register the trigger locally after the binding succeeds.
        if let Err(e) = self.sdk.register_trigger(registration) {
            tracing::error!("Failed to register trigger {}: {}", info.name, e);

            // Best-effort rollback to clear the binding if registration fails.
            // Use Some("") (TriggerUpdate::Clear) — None is TriggerUpdate::Noop and skips the write.
            let rollback_result = match entity_type {
                "collection" => self.sdk.things_set_collection_trigger_uuid(
                    &self.device_id,
                    &bind_uuid,
                    Some(""),
                ),
                "thing" => {
                    self.sdk
                        .things_set_thing_trigger_uuid(&self.device_id, &bind_uuid, Some(""))
                }
                _ => unreachable!(),
            };
            if let Err(rollback_error) = rollback_result {
                tracing::warn!(
                    "Failed to rollback trigger binding for {} {}: {}",
                    entity_type,
                    bind_uuid,
                    rollback_error
                );
            }

            return Err(format!("Failed to register trigger: {}", e));
        }

        tracing::info!(
            "Trigger {} ({}) registered locally via external tool",
            info.name,
            trigger_uuid
        );

        // Keep trigger_bindings table consistent for scheduling/analytics.
        if let Err(e) = self
            .sdk
            .upsert_trigger_binding(&trigger_uuid, entity_type, &bind_uuid)
        {
            tracing::warn!(
                "Failed to upsert trigger binding {} -> {} {}: {}",
                trigger_uuid,
                entity_type,
                bind_uuid,
                e
            );
        }

        if let Some(old_trigger_uuid) = existing_trigger_uuid {
            if old_trigger_uuid != trigger_uuid {
                if let Err(e) = self.sdk.delete_trigger_if_unbound(&old_trigger_uuid) {
                    tracing::warn!(
                        "Failed to delete old trigger {} after rebinding: {}",
                        old_trigger_uuid,
                        e
                    );
                }
            }
        }

        tracing::info!(
            "Trigger {} bound to {} {}",
            trigger_uuid,
            entity_type,
            bind_uuid
        );

        // Resume value: keep consistent with other handlers.
        Ok(json!({
            "confirmed": true,
            "trigger_uuid": trigger_uuid,
            "path": format!("/trigger/{}", trigger_uuid),
            "name": info.name,
            "bound_to": {
                "type": entity_type,
                "uuid": bind_uuid,
            },
        }))
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Trigger Rule Deleted Handler
// ══════════════════════════════════════════════════════════════════════════════

/// Payload for trigger_rule_deleted - matches agent's interrupt data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRuleDeleted {
    pub trigger_uuid: String,
    #[serde(default)]
    pub bind_uuid: Option<String>,
    #[serde(default)]
    pub bind_type: Option<String>, // "thing" or "collection"
}

/// Handler for trigger_rule_deleted - unbinds and removes a trigger locally
pub struct TriggerRuleDeletedHandler {
    sdk: Arc<TriggerSdk>,
    device_id: String,
}

impl TriggerRuleDeletedHandler {
    pub fn new(sdk: Arc<TriggerSdk>, device_id: String) -> Self {
        Self { sdk, device_id }
    }
}

#[async_trait]
impl ExternalToolHandler for TriggerRuleDeletedHandler {
    async fn handle(&self, interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        tracing::debug!(
            interrupt_id = %interrupt_id,
            payload = %payload,
            "[TriggerRuleDeletedHandler] Received external tool call"
        );
        let data = payload.clone();
        let info: TriggerRuleDeleted = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse trigger_rule_deleted: {}", e))?;

        let trigger_uuid = normalize_required_uuid(&info.trigger_uuid, "trigger_uuid")?;
        let bind_uuid = normalize_optional_string(info.bind_uuid);
        let bind_type = normalize_optional_string(info.bind_type).map(|value| value.to_lowercase());

        if let Some(bind_type) = bind_type.as_deref() {
            if bind_type != "thing" && bind_type != "collection" {
                return Err(format!(
                    "Invalid bind_type '{}'; expected 'thing' or 'collection'",
                    bind_type
                ));
            }
        }

        tracing::info!(
            trigger_uuid = %trigger_uuid,
            bind_uuid = ?bind_uuid,
            bind_type = ?bind_type,
            "[TriggerRuleDeletedHandler] Deleting trigger"
        );

        // delete_trigger_and_bindings clears the CRDT on every bound entity,
        // removes all trigger_bindings rows, and force-deletes the trigger record.
        let deleted = self
            .sdk
            .delete_trigger_and_bindings(&self.device_id, &trigger_uuid)
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "delete_trigger_and_bindings failed for {}: {}",
                    trigger_uuid,
                    e
                );
                false
            });

        tracing::info!(
            trigger_uuid = %trigger_uuid,
            deleted = %deleted,
            "Trigger deleted via external tool"
        );

        let mut response = json!({
            "confirmed": true,
            "trigger_uuid": trigger_uuid,
            "deleted": deleted,
        });

        if let (Some(bind_type), Some(bind_uuid)) = (bind_type, bind_uuid) {
            response["unbound_from"] = json!({
                "type": bind_type,
                "uuid": bind_uuid,
            });
        }

        Ok(response)
    }
}

// ══════════════════════════════════════════════════════════════════════════════�?
// Registry Builder Helper
// ══════════════════════════════════════════════════════════════════════════════�?

// ══════════════════════════════════════════════════════════════════════════════
// Triggers List Handler
// ══════════════════════════════════════════════════════════════════════════════

/// Handler for triggers_list_request - returns local triggers snapshot
pub struct TriggersListHandler {
    sdk: Arc<TriggerSdk>,
}

impl TriggersListHandler {
    pub fn new(sdk: Arc<TriggerSdk>) -> Self {
        Self { sdk }
    }
}

#[async_trait]
impl ExternalToolHandler for TriggersListHandler {
    async fn handle(&self, _interrupt_id: &str, _payload: &JsonValue) -> Result<JsonValue, String> {
        let json_str = self
            .sdk
            .list_triggers_json()
            .map_err(|e| format!("Failed to list triggers: {e}"))?;

        let triggers: JsonValue = serde_json::from_str(&json_str)
            .map_err(|e| format!("Failed to parse triggers JSON: {e}"))?;

        tracing::info!("Triggers list provided via external tool");

        Ok(json!({ "triggers": triggers }))
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Trigger Test Request Handler
// ══════════════════════════════════════════════════════════════════════════════

/// Payload for trigger_test_request
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerTestRequest {
    pub trigger_json: String,
    #[serde(default)]
    pub start_iso: Option<String>,
    #[serde(default)]
    pub end_iso: Option<String>,
    #[serde(default)]
    pub manual: bool,
}

/// Handler for trigger_test_request - runs trigger simulation against local events
pub struct TriggerTestRequestHandler {
    sdk: Arc<TriggerSdk>,
}

impl TriggerTestRequestHandler {
    pub fn new(sdk: Arc<TriggerSdk>) -> Self {
        Self { sdk }
    }
}

#[async_trait]
impl ExternalToolHandler for TriggerTestRequestHandler {
    async fn handle(&self, _interrupt_id: &str, payload: &JsonValue) -> Result<JsonValue, String> {
        let data = payload.clone();
        let req: TriggerTestRequest = serde_json::from_value(data)
            .map_err(|e| format!("Failed to parse trigger_test_request: {e}"))?;

        if req.trigger_json.trim().is_empty() {
            return Err("trigger_json is required".to_string());
        }

        let result_json = self
            .sdk
            .trigger_test_json(&req.trigger_json, req.start_iso, req.end_iso, req.manual)
            .map_err(|e| format!("Trigger test failed: {e}"))?;

        let result: JsonValue = serde_json::from_str(&result_json)
            .map_err(|e| format!("Failed to parse trigger test result: {e}"))?;

        tracing::info!("Trigger test completed via external tool");

        Ok(result)
    }
}

/// Register all built-in things handlers on the unified external tool executor.
pub fn register_things_external_tools(
    executor: &mut ExternalToolExecutor,
    sdk: Arc<TriggerSdk>,
    device_id: &str,
) {
    let device_id = device_id.to_string();

    executor.register(
        "things_collection_added",
        CollectionAddedHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "things_collection_edited",
        CollectionEditedHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "things_collection_removed",
        CollectionRemovedHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "things_thing_added",
        ThingAddedHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "things_thing_edited",
        ThingEditedHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "things_thing_removed",
        ThingRemovedHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "things_thing_content_edit",
        ThingContentEditHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "things_thing_moved",
        ThingMovedHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "things_list_snapshot_request",
        ThingsListSnapshotHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "things_get_thing_markdown_request",
        ThingsGetThingMarkdownHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "virtual_fs_ls_request",
        VirtualFsLsHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "virtual_fs_tree_request",
        VirtualFsTreeHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "virtual_fs_cat_request",
        VirtualFsCatHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "virtual_fs_edit_request",
        VirtualFsEditHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "virtual_fs_delete_request",
        VirtualFsDeleteHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "virtual_fs_move_request",
        VirtualFsMoveHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "virtual_fs_create_request",
        VirtualFsCreateHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "events_retrieve_request",
        EventsRetrieveRequestHandler::new(sdk.clone()),
    );
    executor.register(
        "events_abstract_request",
        EventsAbstractRequestHandler::new(sdk.clone()),
    );
    executor.register(
        "trigger_rule_published",
        TriggerRulePublishedHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "trigger_rule_deleted",
        TriggerRuleDeletedHandler::new(sdk.clone(), device_id.clone()),
    );
    executor.register(
        "triggers_list_request",
        TriggersListHandler::new(sdk.clone()),
    );
    executor.register(
        "trigger_test_request",
        TriggerTestRequestHandler::new(sdk.clone()),
    );
}

fn handler_error_json(error: anyhow::Error) -> String {
    serde_json::from_str::<JsonValue>(&error.to_string())
        .map(|value| value.to_string())
        .unwrap_or_else(|_| {
            json!({
                "error": "handler_failed",
                "message": error.to_string(),
            })
            .to_string()
        })
}

async fn load_image_part(uri: &str) -> Result<ToolImagePart, String> {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return fetch_http_image(uri).await;
    }

    if uri.starts_with("remi://") {
        let parsed = RemiUri::parse(uri).map_err(|error| error.to_string())?;
        return match parsed.location {
            RemiUriLocation::Remote => fetch_http_image(&parsed.path).await,
            RemiUriLocation::File => {
                let path = if parsed.path.len() > 2 && parsed.path.chars().nth(1) == Some(':') {
                    parsed.path.clone()
                } else {
                    format!("/{}", parsed.path)
                };
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|e| format!("Cannot read {path}: {e}"))?;
                Ok(ToolImagePart {
                    media_type: parsed.mime_type,
                    data: bytes,
                    exif: None,
                })
            }
            RemiUriLocation::Local => Err(format!(
                "remi://local URIs require app data dir context and cannot be resolved by the generic SDK handler: {uri}"
            )),
            RemiUriLocation::Inline => Err(format!(
                "remi://inline URIs are not supported by cat_tool yet: {uri}"
            )),
        };
    }

    let media_type = mime_from_extension(uri.rsplit('.').next().unwrap_or("jpg")).to_string();
    let bytes = tokio::fs::read(uri)
        .await
        .map_err(|error| format!("Failed to read local image {uri}: {error}"))?;
    Ok(ToolImagePart {
        media_type,
        data: bytes,
        exif: None,
    })
}

async fn fetch_http_image(url: &str) -> Result<ToolImagePart, String> {
    let response = reqwest::get(url)
        .await
        .map_err(|error| format!("Failed to fetch image {url}: {error}"))?;
    let media_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| mime_from_extension(url.rsplit('.').next().unwrap_or("jpg")))
        .to_string();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("Failed to read image response {url}: {error}"))?;
    Ok(ToolImagePart {
        media_type,
        data: bytes.to_vec(),
        exif: None,
    })
}
