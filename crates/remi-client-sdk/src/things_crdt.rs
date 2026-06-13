use anyhow::{Context, Result};
use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ObjId, ObjType, ReadDoc, ScalarValue, Value as AmValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

use crate::things_events::{ThingsDocumentChangeKind, ThingsDocumentEvent};

pub use remi_things_crdt::{
    ContentEntry, ContentEntryKind, ContentEntryPayload, ContentEntryUpdate, DateField, ImageField,
    JsonObjectField, LocationField, ThingDatatype, UrlField,
};

use remi_things_crdt::{
    CURRENT_SCHEMA_VERSION,
    CollectionDocView,
    // V3 types
    CollectionOp,
    Content,
    CrdtDataType,
    DEFAULT_COMPACTION_THRESHOLD,
    Op,
    ROOT_DOC_UUID,
    RootView,
    Schema,
    // V3 built-in fields (multi-value)
    ThingBuiltInFieldsUpdate,
    ThingContentView,
    ThingMarkdownOp,
    ThingMarkdownView,
    TriggerUpdate,
    apply_collection_op,
    apply_thing_markdown_op,
    compact_collection_doc,
    compact_root_doc,
    compact_thing_content_doc,
    extract_collection_doc_view,
    extract_root_view,
    extract_thing_content_view,
    extract_thing_markdown_view,
    // V3 compaction
    needs_compaction,
};

const INTERNAL_CREATED_AT_ATTR_KEY: &str = "__remi_created_at";
const INTERNAL_UPDATED_AT_ATTR_KEY: &str = "__remi_updated_at";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct EntityTimestamps {
    created_at: Option<String>,
    updated_at: Option<String>,
}

fn normalize_timestamp_value(value: Option<String>) -> Option<String> {
    value.and_then(|item| {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn now_timestamp_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn extract_entity_timestamps(attrs: Option<&Value>) -> EntityTimestamps {
    let Some(Value::Object(map)) = attrs else {
        return EntityTimestamps::default();
    };

    EntityTimestamps {
        created_at: normalize_timestamp_value(
            map.get(INTERNAL_CREATED_AT_ATTR_KEY)
                .and_then(|value| value.as_str())
                .map(str::to_string),
        ),
        updated_at: normalize_timestamp_value(
            map.get(INTERNAL_UPDATED_AT_ATTR_KEY)
                .and_then(|value| value.as_str())
                .map(str::to_string),
        ),
    }
}

fn strip_internal_timestamp_attrs(attrs: Option<&Value>) -> Option<Value> {
    match attrs {
        Some(Value::Object(map)) => {
            let filtered = map
                .iter()
                .filter(|(key, _)| {
                    *key != INTERNAL_CREATED_AT_ATTR_KEY && *key != INTERNAL_UPDATED_AT_ATTR_KEY
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<serde_json::Map<String, Value>>();

            if filtered.is_empty() {
                None
            } else {
                Some(Value::Object(filtered))
            }
        }
        Some(other) => Some(other.clone()),
        None => None,
    }
}

fn extract_collection_card_jsx(attrs: Option<&Value>) -> Option<String> {
    attrs
        .and_then(|value| value.as_object())
        .and_then(|map| map.get("card_jsx"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn merge_entity_timestamps_into_attrs(
    attrs: Option<&Value>,
    created_at: Option<String>,
    updated_at: Option<String>,
) -> Result<String> {
    let mut map = match attrs {
        Some(Value::Object(existing)) => existing.clone(),
        Some(_) => serde_json::Map::new(),
        None => serde_json::Map::new(),
    };

    match normalize_timestamp_value(created_at) {
        Some(value) => {
            map.insert(
                INTERNAL_CREATED_AT_ATTR_KEY.to_string(),
                Value::String(value),
            );
        }
        None => {
            map.remove(INTERNAL_CREATED_AT_ATTR_KEY);
        }
    }

    match normalize_timestamp_value(updated_at) {
        Some(value) => {
            map.insert(
                INTERNAL_UPDATED_AT_ATTR_KEY.to_string(),
                Value::String(value),
            );
        }
        None => {
            map.remove(INTERNAL_UPDATED_AT_ATTR_KEY);
        }
    }

    serde_json::to_string(&Value::Object(map)).context("Failed to encode timestamp attrs")
}

fn resolve_entity_timestamps(
    existing_attrs: Option<&Value>,
    provided_created_at: Option<String>,
    provided_updated_at: Option<String>,
    existed: bool,
) -> EntityTimestamps {
    let existing = extract_entity_timestamps(existing_attrs);
    let fallback_now = now_timestamp_rfc3339();

    let created_at = normalize_timestamp_value(provided_created_at)
        .or(existing.created_at)
        .unwrap_or_else(|| fallback_now.clone());
    let updated_at = normalize_timestamp_value(provided_updated_at).unwrap_or_else(|| {
        if existed {
            fallback_now.clone()
        } else {
            created_at.clone()
        }
    });

    EntityTimestamps {
        created_at: Some(created_at),
        updated_at: Some(updated_at),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotOptions {
    /// If false, omit thing `data.content` from the snapshot (and avoid extracting content when paired with ExtractOptions).
    pub include_content: bool,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            include_content: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThingCollectionEntry {
    pub uuid: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_jsx: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// "user" or "application" — populated from server-side actor metadata cache
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_type: Option<String>,
    /// App ID when actor_type is "application"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_app_id: Option<String>,
    /// Resolved display name for the app, if available
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingCollectionUpsert {
    pub uuid: String,
    pub title: String,
    /// Tri-state semantics:
    /// - key omitted / null => no change
    /// - empty string => clear
    /// - UUID string => set
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_uuid: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThingEntry {
    pub uuid: String,
    pub title: String,
    pub datatype: ThingDatatype,
    pub data: Value,
    pub collection_uuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_uuid: Option<String>,
    #[serde(default)]
    pub parent_uuid: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Status of the thing: "none", "in-progress", "stalled", "done"
    #[serde(default)]
    pub status: String,
    /// Timestamp when status was last changed (ms since epoch)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_timestamp_ms: Option<i64>,
    /// "user" or "application" — populated from server-side actor metadata cache
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_type: Option<String>,
    /// App ID when actor_type is "application"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_app_id: Option<String>,
    /// Resolved display name for the app, if available
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_display_name: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingUpsert {
    pub uuid: String,
    pub title: String,
    pub datatype: ThingDatatype,
    /// Optional to support CRDT-typed markdown edits where text is updated via SpliceText ops.
    /// When omitted/null, we won't rewrite `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    pub collection_uuid: String,
    /// Tri-state semantics:
    /// - key omitted / null => no change
    /// - empty string => clear
    /// - UUID string => set
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_uuid: Option<String>,
    #[serde(default)]
    pub parent_uuid: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingsSnapshot {
    pub collections: Vec<ThingCollectionEntry>,
    pub things: Vec<ThingEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingsSnapshotState {
    pub collections: Vec<ThingCollectionEntry>,
    pub things: Vec<ThingEntry>,
    pub dirty: bool,
    pub last_sync_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TreeCollectionData {
    pub uuid: String,
    pub title: String,
    pub trigger_uuid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TreeThingData {
    pub uuid: String,
    pub title: String,
    pub status: String,
    pub trigger_uuid: Option<String>,
    pub collection_uuid: String,
    pub parent_uuid: Option<String>,
    pub entries: Vec<ContentEntry>,
}

#[derive(Debug, Clone)]
pub struct ThingsTreeData {
    pub collections: Vec<TreeCollectionData>,
    pub things: Vec<TreeThingData>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ContentTypeRegistry;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegisteredContentType {
    Markdown,
    JsonObject,
    Url,
    Location,
    Date,
    Image,
    Custom(String),
}

impl ContentTypeRegistry {
    pub fn new() -> Self {
        Self
    }

    fn detect_payload_type(&self, value: &Value) -> Result<RegisteredContentType> {
        let payload_type = value
            .get("type")
            .and_then(|entry_type| entry_type.as_str())
            .filter(|entry_type| !entry_type.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing payload type"))?;

        match payload_type {
            "markdown" => Ok(RegisteredContentType::Markdown),
            "json_object" => Ok(RegisteredContentType::JsonObject),
            "url" => Ok(RegisteredContentType::Url),
            "location" => Ok(RegisteredContentType::Location),
            "date" => Ok(RegisteredContentType::Date),
            "image" => Ok(RegisteredContentType::Image),
            other => Ok(RegisteredContentType::Custom(other.to_string())),
        }
    }

    pub fn parse_content_entry(&self, value: &Value) -> Result<ContentEntry> {
        let id = value
            .get("id")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let title = value
            .get("title")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string());
        let order = value
            .get("order")
            .and_then(|item| item.as_f64())
            .unwrap_or(0.0);
        let payload_value = value
            .get("payload")
            .ok_or_else(|| anyhow::anyhow!("Missing payload"))?;

        Ok(ContentEntry {
            id,
            title,
            order,
            payload: self.parse_content_entry_payload(payload_value)?,
        })
    }

    pub fn parse_content_entry_update(
        &self,
        value: &Value,
    ) -> Result<(
        Option<Option<String>>,
        Option<f64>,
        Option<ContentEntryPayload>,
    )> {
        let title = if value.get("title").is_some() {
            Some(
                value
                    .get("title")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string()),
            )
        } else {
            None
        };
        let order = value.get("order").and_then(|item| item.as_f64());
        let payload = value
            .get("payload")
            .map(|payload| self.parse_content_entry_payload(payload))
            .transpose()?;

        Ok((title, order, payload))
    }

    pub fn parse_content_entry_payload(&self, value: &Value) -> Result<ContentEntryPayload> {
        match self.detect_payload_type(value)? {
            RegisteredContentType::Markdown => self.parse_markdown_payload(value),
            RegisteredContentType::JsonObject => self.parse_json_object_payload(value),
            RegisteredContentType::Url => self.parse_url_payload(value),
            RegisteredContentType::Location => self.parse_location_payload(value),
            RegisteredContentType::Date => self.parse_date_payload(value),
            RegisteredContentType::Image => self.parse_image_payload(value),
            RegisteredContentType::Custom(content_type) => {
                self.parse_custom_payload(&content_type, value)
            }
        }
    }

    pub fn serialize_content_entry(&self, entry: &ContentEntry) -> Value {
        json!({
            "id": entry.id,
            "title": entry.title,
            "order": entry.order,
            "payload": self.serialize_content_entry_payload(&entry.payload),
        })
    }

    pub fn serialize_content_entries(&self, entries: &[ContentEntry]) -> Value {
        Value::Array(
            entries
                .iter()
                .map(|entry| self.serialize_content_entry(entry))
                .collect(),
        )
    }

    pub fn serialize_thing_built_in(
        &self,
        built_in: &remi_things_crdt::ThingBuiltInFieldsView,
    ) -> Value {
        let mut obj = serde_json::Map::new();
        if !built_in.content_entries.is_empty() {
            obj.insert(
                "content_entries".to_string(),
                self.serialize_content_entries(&built_in.content_entries),
            );
        }
        if let Some(extra) = &built_in.extra {
            obj.insert("extra".to_string(), extra.clone());
        }
        Value::Object(obj)
    }

    pub fn serialize_thing_snapshot_data(
        &self,
        meta: &remi_things_crdt::ThingMetaView,
        content: Option<&remi_things_crdt::view::ContentView>,
        options: SnapshotOptions,
    ) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("status".to_string(), json!(meta.status));
        obj.insert("datatype".to_string(), json!(meta.datatype));
        obj.insert(
            "attrs".to_string(),
            json!(strip_internal_timestamp_attrs(meta.attrs.as_ref())),
        );
        if options.include_content {
            obj.insert("content".to_string(), json!(content));
        }
        obj.insert(
            "built_in".to_string(),
            self.serialize_thing_built_in(&meta.built_in),
        );
        Value::Object(obj)
    }

    pub fn find_first_payload_by_kind(
        &self,
        entries: &[ContentEntry],
        kind: &remi_things_crdt::ContentEntryKind,
    ) -> Option<Value> {
        entries
            .iter()
            .find(|entry| &entry.kind() == kind)
            .map(|entry| self.serialize_content_entry_payload(&entry.payload))
    }

    fn parse_markdown_payload(&self, value: &Value) -> Result<ContentEntryPayload> {
        let doc_uuid = value
            .get("doc_uuid")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        Ok(ContentEntryPayload::Markdown { doc_uuid })
    }

    fn parse_json_object_payload(&self, value: &Value) -> Result<ContentEntryPayload> {
        let data_doc_uuid = value
            .get("data_doc_uuid")
            .and_then(|item| item.as_str())
            .filter(|item| !item.trim().is_empty())
            .map(|item| item.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let schema_doc_uuid = value
            .get("schema_doc_uuid")
            .and_then(|item| item.as_str())
            .filter(|item| !item.trim().is_empty())
            .map(|item| item.to_string());
        Ok(ContentEntryPayload::JsonObject(JsonObjectField {
            data_doc_uuid,
            schema_doc_uuid,
        }))
    }

    fn parse_url_payload(&self, value: &Value) -> Result<ContentEntryPayload> {
        let url = value
            .get("url")
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .to_string();
        let title = value
            .get("title")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string());
        let description = value
            .get("description")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string());
        let image_url = value
            .get("image_url")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string());
        let favicon_url = value
            .get("favicon_url")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string());
        let site_name = value
            .get("site_name")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string());
        let resolved = value
            .get("resolved")
            .and_then(|item| item.as_bool())
            .unwrap_or(false);
        Ok(ContentEntryPayload::Url(remi_things_crdt::UrlField {
            url,
            title,
            description,
            image_url,
            favicon_url,
            site_name,
            resolved,
        }))
    }

    fn parse_location_payload(&self, value: &Value) -> Result<ContentEntryPayload> {
        use remi_things_crdt::LocationField;

        let loc_type = value
            .get("loc_type")
            .and_then(|entry_type| entry_type.as_str())
            .unwrap_or("");
        let location = match loc_type {
            "coordinate" => {
                let lat = value
                    .get("lat")
                    .and_then(|item| item.as_f64())
                    .ok_or_else(|| anyhow::anyhow!("Missing lat"))?;
                let lng = value
                    .get("lng")
                    .and_then(|item| item.as_f64())
                    .ok_or_else(|| anyhow::anyhow!("Missing lng"))?;
                let coord_system = value
                    .get("coord_system")
                    .and_then(|item| item.as_str())
                    .unwrap_or("wgs84")
                    .to_string();
                let source_name = value
                    .get("source_name")
                    .and_then(|item| item.as_str())
                    .map(|item| item.to_string());
                LocationField::Coordinate {
                    lat,
                    lng,
                    coord_system,
                    source_name,
                }
            }
            "fuzzy" => {
                let name = value
                    .get("name")
                    .and_then(|item| item.as_str())
                    .unwrap_or("")
                    .to_string();
                let place_type = value
                    .get("place_type")
                    .and_then(|item| item.as_str())
                    .unwrap_or("")
                    .to_string();
                LocationField::Fuzzy { name, place_type }
            }
            _ => anyhow::bail!("Invalid location type: {}", loc_type),
        };

        Ok(ContentEntryPayload::Location(location))
    }

    fn parse_date_payload(&self, value: &Value) -> Result<ContentEntryPayload> {
        use remi_things_crdt::DateField;

        let timestamp_ms = value
            .get("timestamp_ms")
            .and_then(|item| item.as_i64())
            .ok_or_else(|| anyhow::anyhow!("Missing timestamp_ms"))?;
        let has_time = value
            .get("has_time")
            .and_then(|item| item.as_bool())
            .unwrap_or(false);
        let timezone = value
            .get("timezone")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string());
        Ok(ContentEntryPayload::Date(DateField {
            timestamp_ms,
            has_time,
            timezone,
        }))
    }

    fn parse_image_payload(&self, value: &Value) -> Result<ContentEntryPayload> {
        let uri = value
            .get("uri")
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .to_string();
        let caption = value
            .get("caption")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string());
        let width = value
            .get("width")
            .and_then(|item| item.as_u64())
            .map(|item| item as u32);
        let height = value
            .get("height")
            .and_then(|item| item.as_u64())
            .map(|item| item as u32);
        let size_bytes = value.get("size_bytes").and_then(|item| item.as_u64());
        let device_id = value
            .get("device_id")
            .and_then(|item| item.as_str())
            .map(|item| item.to_string());
        Ok(ContentEntryPayload::Image(remi_things_crdt::ImageField {
            uri,
            caption,
            width,
            height,
            size_bytes,
            device_id,
        }))
    }

    // External custom payload contract:
    // - Legacy explicit custom wrapper uses {"type":"custom","data":<json>}.
    // - Future extension types use {"type":"<external-type>", ...}.
    //   For object payloads, every field except `type` is preserved verbatim.
    //   For scalar/array payloads, callers should wrap the value as
    //   {"type":"<external-type>","data":<json>} so round-tripping stays lossless.
    fn parse_custom_payload(
        &self,
        content_type: &str,
        value: &Value,
    ) -> Result<ContentEntryPayload> {
        let data = if content_type == "custom" {
            value.get("data").cloned().unwrap_or(Value::Null)
        } else {
            match value {
                Value::Object(map) => {
                    let mut data = serde_json::Map::new();
                    for (key, field_value) in map {
                        if key != "type" {
                            data.insert(key.clone(), field_value.clone());
                        }
                    }
                    Value::Object(data)
                }
                _ => Value::Null,
            }
        };

        Ok(ContentEntryPayload::Custom {
            content_type: content_type.to_string(),
            data,
        })
    }

    pub fn serialize_content_entry_payload(&self, payload: &ContentEntryPayload) -> Value {
        match payload {
            ContentEntryPayload::Markdown { doc_uuid } => json!({
                "type": "markdown",
                "doc_uuid": doc_uuid,
            }),
            ContentEntryPayload::JsonObject(field) => json!({
                "type": "json_object",
                "data_doc_uuid": field.data_doc_uuid,
                "schema_doc_uuid": field.schema_doc_uuid,
            }),
            ContentEntryPayload::Url(url) => json!({
                "type": "url",
                "url": url.url,
                "title": url.title,
                "description": url.description,
                "image_url": url.image_url,
                "favicon_url": url.favicon_url,
                "site_name": url.site_name,
                "resolved": url.resolved,
            }),
            ContentEntryPayload::Location(location) => self.serialize_location_payload(location),
            ContentEntryPayload::Date(date) => json!({
                "type": "date",
                "timestamp_ms": date.timestamp_ms,
                "has_time": date.has_time,
                "timezone": date.timezone,
            }),
            ContentEntryPayload::Image(image) => json!({
                "type": "image",
                "uri": image.uri,
                "caption": image.caption,
                "width": image.width,
                "height": image.height,
                "size_bytes": image.size_bytes,
                "device_id": image.device_id,
            }),
            ContentEntryPayload::Custom { content_type, data } => {
                self.serialize_custom_payload(content_type, data)
            }
        }
    }

    // Serialize back to the external SDK/API shape described above.
    fn serialize_custom_payload(&self, content_type: &str, data: &Value) -> Value {
        if content_type == "custom" {
            return json!({
                "type": "custom",
                "data": data,
            });
        }

        match data {
            Value::Object(map) => {
                let mut obj = serde_json::Map::new();
                obj.insert("type".to_string(), Value::String(content_type.to_string()));
                for (key, value) in map {
                    obj.insert(key.clone(), value.clone());
                }
                Value::Object(obj)
            }
            other => json!({
                "type": content_type,
                "data": other,
            }),
        }
    }

    fn serialize_location_payload(&self, location: &remi_things_crdt::LocationField) -> Value {
        match location {
            remi_things_crdt::LocationField::Coordinate {
                lat,
                lng,
                coord_system,
                source_name,
            } => json!({
                "type": "location",
                "loc_type": "coordinate",
                "lat": lat,
                "lng": lng,
                "coord_system": coord_system,
                "source_name": source_name,
            }),
            remi_things_crdt::LocationField::Fuzzy { name, place_type } => json!({
                "type": "location",
                "loc_type": "fuzzy",
                "name": name,
                "place_type": place_type,
            }),
        }
    }

    pub fn extract_markdown_text_from_snapshot_data(&self, data: &Value) -> Option<String> {
        if let Some(markdown) = data.get("markdown").and_then(|value| value.as_str()) {
            return Some(markdown.to_string());
        }

        data.get("content")
            .and_then(|content| content.get("blocks"))
            .and_then(|blocks| blocks.as_array())
            .and_then(|blocks| {
                blocks
                    .iter()
                    .find(|block| block.get("id") == Some(&Value::String("main".to_string())))
            })
            .and_then(|block| block.get("text"))
            .and_then(|text| text.as_str())
            .map(|text| text.to_string())
    }

    pub fn extract_thing_snapshot_parts(
        &self,
        data: &Value,
    ) -> Result<(Option<String>, Vec<ContentEntry>)> {
        Ok((
            self.extract_markdown_text_from_snapshot_data(data),
            self.extract_content_entries_from_snapshot_data(data)?,
        ))
    }

    pub fn markdown_content_from_value(
        &self,
        original_datatype: &ThingDatatype,
        payload: &Value,
    ) -> Content {
        if original_datatype.is_markdownish() {
            let text = match payload {
                Value::String(s) => s.clone(),
                Value::Object(map) => map
                    .get("markdown")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| payload.to_string()),
                _ => payload.to_string(),
            };
            return Content::Markdown {
                blocks: vec![remi_things_crdt::Block {
                    id: "main".to_string(),
                    r#type: "markdown".to_string(),
                    attrs_json: None,
                    text: Some(text),
                }],
            };
        }

        let attrs = json!({
            "embed_kind": original_datatype.as_str(),
            "payload": payload,
        })
        .to_string();

        Content::Markdown {
            blocks: vec![remi_things_crdt::Block {
                id: "main".to_string(),
                r#type: original_datatype.to_string(),
                attrs_json: Some(attrs),
                text: None,
            }],
        }
    }

    pub fn extract_content_entries_from_snapshot_data(
        &self,
        data: &Value,
    ) -> Result<Vec<ContentEntry>> {
        let Some(entries) = data
            .get("built_in")
            .and_then(|built_in| built_in.get("content_entries"))
            .and_then(|entries| entries.as_array())
        else {
            return Ok(Vec::new());
        };

        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let payload = entry
                .get("payload")
                .ok_or_else(|| anyhow::anyhow!("Missing payload in stashed content entry"))?;

            out.push(ContentEntry {
                id: entry
                    .get("id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing id in stashed content entry"))?
                    .to_string(),
                title: entry
                    .get("title")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                order: entry
                    .get("order")
                    .and_then(|value| value.as_f64())
                    .unwrap_or(0.0),
                payload: self.parse_content_entry_payload(payload)?,
            });
        }

        Ok(out)
    }
}

pub fn init_empty_doc(device_id: &str) -> Result<Vec<u8>> {
    let mut doc = AutoCommit::new();
    Schema::ensure_root(&mut doc, device_id, 0)?;
    Ok(doc.save())
}

pub fn upsert_collection(
    doc_bytes: &[u8],
    device_id: &str,
    upsert: ThingCollectionUpsert,
) -> Result<Vec<u8>> {
    let trigger = trigger_update_from_tri_state(upsert.trigger_uuid.as_deref());
    remi_things_crdt::apply_op(
        doc_bytes,
        device_id,
        Op::UpsertCollection {
            id: upsert.uuid,
            title: Some(upsert.title),
            status: None,
            trigger,
        },
    )
    .context("Failed to apply v2 UpsertCollection")
}

pub fn delete_collection(
    doc_bytes: &[u8],
    device_id: &str,
    collection_uuid: &str,
) -> Result<Vec<u8>> {
    remi_things_crdt::apply_op(
        doc_bytes,
        device_id,
        Op::DeleteCollection {
            id: collection_uuid.to_string(),
        },
    )
    .context("Failed to apply v2 DeleteCollection")
}

pub fn upsert_thing(doc_bytes: &[u8], device_id: &str, upsert: ThingUpsert) -> Result<Vec<u8>> {
    let trigger = trigger_update_from_tri_state(upsert.trigger_uuid.as_deref());
    let content_registry = ContentTypeRegistry::new();

    let content = upsert
        .data
        .as_ref()
        .map(|payload| content_registry.markdown_content_from_value(&upsert.datatype, payload));
    remi_things_crdt::apply_op(
        doc_bytes,
        device_id,
        Op::UpsertThing {
            id: upsert.uuid,
            collection_id: upsert.collection_uuid,
            datatype: Some(ThingDatatype::Markdown),
            status: None,
            status_timestamp_ms: None,
            title: Some(upsert.title),
            parent_id: upsert
                .parent_uuid
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            trigger,
            content,
        },
    )
    .context("Failed to apply v2 UpsertThing")
}

pub fn splice_text(
    doc_bytes: &[u8],
    device_id: &str,
    thing_id: &str,
    block_id: &str,
    index: usize,
    delete: usize,
    insert: &str,
) -> Result<Vec<u8>> {
    remi_things_crdt::apply_op(
        doc_bytes,
        device_id,
        Op::SpliceText {
            thing_id: thing_id.to_string(),
            block_id: block_id.to_string(),
            index,
            delete,
            insert: insert.to_string(),
        },
    )
    .context("Failed to apply v2 SpliceText")
}

pub fn markdown_only_content_from_value(
    original_datatype: &ThingDatatype,
    payload: &Value,
) -> Content {
    ContentTypeRegistry::new().markdown_content_from_value(original_datatype, payload)
}

pub fn delete_thing(
    doc_bytes: &[u8],
    device_id: &str,
    collection_uuid: &str,
    thing_uuid: &str,
) -> Result<Vec<u8>> {
    let _ = collection_uuid; // v2 delete is global by id.
    remi_things_crdt::apply_op(
        doc_bytes,
        device_id,
        Op::DeleteThing {
            id: thing_uuid.to_string(),
        },
    )
    .context("Failed to apply v2 DeleteThing")
}

pub fn set_thing_status(
    doc_bytes: &[u8],
    device_id: &str,
    thing_uuid: &str,
    status: &str,
    timestamp_ms: Option<i64>,
) -> Result<Vec<u8>> {
    remi_things_crdt::apply_op(
        doc_bytes,
        device_id,
        Op::SetThingStatus {
            id: thing_uuid.to_string(),
            status: status.to_string(),
            timestamp_ms,
        },
    )
    .context("Failed to apply v2 SetThingStatus")
}

pub fn extract_snapshot(doc_bytes: &[u8]) -> Result<ThingsSnapshot> {
    extract_snapshot_with_options(doc_bytes, SnapshotOptions::default())
}

pub fn extract_snapshot_with_options(
    doc_bytes: &[u8],
    options: SnapshotOptions,
) -> Result<ThingsSnapshot> {
    let extract_opts = remi_things_crdt::ExtractOptions {
        include_things: true,
        include_content: options.include_content,
    };
    let (view, _scale) =
        remi_things_crdt::extract_view_with_options_and_scale(doc_bytes, extract_opts)
            .context("Failed to extract v2 view")?;
    snapshot_from_view_with_options(&view, options)
}

pub fn snapshot_from_view(view: &remi_things_crdt::view::View) -> Result<ThingsSnapshot> {
    snapshot_from_view_with_options(view, SnapshotOptions::default())
}

pub fn snapshot_from_view_with_options(
    view: &remi_things_crdt::view::View,
    options: SnapshotOptions,
) -> Result<ThingsSnapshot> {
    let mut collections: Vec<ThingCollectionEntry> = Vec::new();
    for c in &view.collections {
        let deleted = c.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false);
        if deleted {
            continue;
        }
        let trigger_uuid = desired_trigger_uuid(deleted, &c.trigger);
        let timestamps = extract_entity_timestamps(c.attrs.as_ref());
        collections.push(ThingCollectionEntry {
            uuid: c.id.clone(),
            title: c.title.clone(),
            trigger_uuid,
            card_jsx: extract_collection_card_jsx(c.attrs.as_ref()),
            created_at: timestamps.created_at.unwrap_or_default(),
            updated_at: timestamps.updated_at.unwrap_or_default(),
            actor_type: None,
            actor_app_id: None,
            actor_display_name: None,
        });
    }

    let mut things: Vec<ThingEntry> = Vec::new();
    for t in &view.things {
        let deleted = t.tombstone.as_ref().map(|x| x.deleted).unwrap_or(false);
        if deleted {
            continue;
        }
        let trigger_uuid = desired_trigger_uuid(deleted, &t.trigger);
        let timestamps = extract_entity_timestamps(t.attrs.as_ref());
        things.push(ThingEntry {
            uuid: t.id.clone(),
            title: t.title.clone().unwrap_or_default(),
            datatype: t.datatype.clone(),
            data: thing_data_from_view_with_options(t, options),
            collection_uuid: t.collection_id.clone(),
            trigger_uuid,
            parent_uuid: t.parent_id.clone(),
            created_at: timestamps.created_at.unwrap_or_default(),
            updated_at: timestamps.updated_at.unwrap_or_default(),
            status: t.status.as_storage_str().to_string(),
            status_timestamp_ms: t.status.timestamp_ms(),
            actor_type: None,
            actor_app_id: None,
            actor_display_name: None,
        });
    }

    Ok(ThingsSnapshot {
        collections,
        things,
    })
}

fn normalize_entity_attrs_value(attrs: Option<Value>) -> Option<Value> {
    match attrs {
        Some(Value::Object(map)) if map.is_empty() => None,
        Some(value) => Some(value),
        None => None,
    }
}

/// Check if document is a supported single-document format (v2 or v3 unified)
pub fn is_v2_doc(doc_bytes: &[u8]) -> bool {
    if doc_bytes.is_empty() {
        return true;
    }

    let Ok(doc) = AutoCommit::load(doc_bytes) else {
        return false;
    };

    match doc.get(automerge::ROOT, Schema::KEY_SCHEMA_VERSION) {
        Ok(Some((AmValue::Scalar(sv), _))) => match sv.as_ref() {
            // Support both legacy v2 and current v3 unified docs
            automerge::ScalarValue::Int(i) => {
                (*i as u32) >= 2 && (*i as u32) <= CURRENT_SCHEMA_VERSION
            }
            _ => false,
        },
        _ => false,
    }
}

pub fn desired_trigger_uuid(
    tombstone_deleted: bool,
    trigger: &Option<remi_things_crdt::view::TriggerBinding>,
) -> Option<String> {
    if tombstone_deleted {
        return None;
    }
    let Some(t) = trigger.as_ref() else {
        return None;
    };
    if t.state != "some" {
        return None;
    }
    t.uuid
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn thing_data_from_view(view: &remi_things_crdt::view::ThingView) -> Value {
    thing_data_from_view_with_options(view, SnapshotOptions::default())
}

pub fn thing_data_from_view_with_options(
    view: &remi_things_crdt::view::ThingView,
    options: SnapshotOptions,
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("status".to_string(), json!(view.status));
    obj.insert("datatype".to_string(), json!(view.datatype));
    obj.insert("attrs".to_string(), json!(view.attrs));
    if options.include_content {
        obj.insert("content".to_string(), json!(view.content));
    }
    Value::Object(obj)
}

pub fn trigger_update_from_tri_state(raw: Option<&str>) -> TriggerUpdate {
    match raw {
        None => TriggerUpdate::Noop,
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                TriggerUpdate::Clear
            } else {
                TriggerUpdate::Set(trimmed.to_string())
            }
        }
    }
}

// ============================================================================
// V3 Multi-Document Architecture
// ============================================================================

/// Document key: (uuid, data_type)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DocumentKey {
    pub uuid: String,
    pub data_type: CrdtDataType,
}

impl DocumentKey {
    pub fn root() -> Self {
        Self {
            uuid: ROOT_DOC_UUID.to_string(),
            data_type: CrdtDataType::Root,
        }
    }

    pub fn collection(uuid: &str) -> Self {
        Self {
            uuid: uuid.to_string(),
            data_type: CrdtDataType::Collection,
        }
    }

    pub fn thing_markdown(uuid: &str) -> Self {
        Self {
            uuid: uuid.to_string(),
            data_type: CrdtDataType::ThingMarkdown,
        }
    }

    pub fn thing_content(uuid: &str) -> Self {
        Self {
            uuid: uuid.to_string(),
            data_type: CrdtDataType::ThingMarkdown,
        }
    }

    pub fn data_type_str(&self) -> &'static str {
        self.data_type.as_str()
    }
}

/// In-memory document with sync state
#[derive(Debug, Clone)]
pub struct DocumentState {
    pub automerge_doc: Vec<u8>,
    pub sync_state: Vec<u8>,
    pub dirty: bool,
    pub last_sync_at: Option<String>,
}

impl DocumentState {
    pub fn new_empty() -> Self {
        Self {
            automerge_doc: Vec::new(),
            sync_state: Vec::new(),
            dirty: false,
            last_sync_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct DocumentStoreView<'a> {
    documents: &'a HashMap<DocumentKey, DocumentState>,
}

impl<'a> DocumentStoreView<'a> {
    fn new(documents: &'a HashMap<DocumentKey, DocumentState>) -> Self {
        Self { documents }
    }

    fn get(&self, key: &DocumentKey) -> Option<&'a DocumentState> {
        self.documents.get(key)
    }

    fn iter(&self) -> impl Iterator<Item = (&'a DocumentKey, &'a DocumentState)> {
        self.documents.iter()
    }
}

struct DocumentStoreMut<'a> {
    device_id: &'a str,
    documents: &'a mut HashMap<DocumentKey, DocumentState>,
}

impl<'a> DocumentStoreMut<'a> {
    fn new(device_id: &'a str, documents: &'a mut HashMap<DocumentKey, DocumentState>) -> Self {
        Self {
            device_id,
            documents,
        }
    }

    fn set(&mut self, key: DocumentKey, state: DocumentState) {
        self.documents.insert(key, state);
    }

    fn set_persisted_state(
        &mut self,
        key: &DocumentKey,
        mark_clean: bool,
        last_sync_at: Option<&str>,
    ) {
        if let Some(state) = self.documents.get_mut(key) {
            if mark_clean {
                state.dirty = false;
            }
            if let Some(last_sync_at) = last_sync_at {
                state.last_sync_at = Some(last_sync_at.to_string());
            }
        }
    }

    fn dirty_document_keys(&self) -> Vec<DocumentKey> {
        self.documents
            .iter()
            .filter(|(_, state)| state.dirty)
            .map(|(key, _)| key.clone())
            .collect()
    }

    fn remove_document(&mut self, key: &DocumentKey) -> Option<DocumentState> {
        self.documents.remove(key)
    }

    fn maybe_compact_with_threshold(
        &mut self,
        key: &DocumentKey,
        threshold: usize,
    ) -> Result<bool> {
        let Some(state) = self.documents.get(key) else {
            return Ok(false);
        };

        if !needs_compaction(&state.automerge_doc, threshold) {
            return Ok(false);
        }

        let compacted = match key.data_type {
            CrdtDataType::Root => compact_root_doc(&state.automerge_doc, self.device_id)
                .context("Failed to compact root document")?,
            CrdtDataType::Collection => {
                compact_collection_doc(&state.automerge_doc, &key.uuid, self.device_id)
                    .context("Failed to compact collection document")?
            }
            CrdtDataType::ThingMarkdown => {
                let doc = AutoCommit::load(&state.automerge_doc)
                    .context("Failed to load thing content document for compaction")?;
                let thing_uuid = match doc.get(automerge::ROOT, "thing_uuid")? {
                    Some((AmValue::Scalar(value), _)) => match value.as_ref() {
                        ScalarValue::Str(value) => value.to_string(),
                        _ => key.uuid.clone(),
                    },
                    _ => key.uuid.clone(),
                };
                compact_thing_content_doc(
                    &state.automerge_doc,
                    &key.uuid,
                    &thing_uuid,
                    self.device_id,
                )
                .context("Failed to compact thing content document")?
            }
        };

        if let Some(state) = self.documents.get_mut(key) {
            state.automerge_doc = compacted;
            state.dirty = true;
        }

        Ok(true)
    }

    fn compact_all_with_threshold(&mut self, threshold: usize) -> Result<usize> {
        let keys_to_compact: Vec<DocumentKey> = self
            .documents
            .iter()
            .filter(|(_, state)| needs_compaction(&state.automerge_doc, threshold))
            .map(|(key, _)| key.clone())
            .collect();

        let mut count = 0;
        for key in keys_to_compact {
            if self.maybe_compact_with_threshold(&key, threshold)? {
                count += 1;
            }
        }

        Ok(count)
    }
}

#[derive(Debug, Clone, Copy)]
struct ThingsDomainReader<'a> {
    store: DocumentStoreView<'a>,
}

impl<'a> ThingsDomainReader<'a> {
    fn new(store: DocumentStoreView<'a>) -> Self {
        Self { store }
    }

    fn root_view(&self) -> Result<RootView> {
        let key = DocumentKey::root();
        match self.store.get(&key) {
            Some(state) => extract_root_view(&state.automerge_doc),
            None => Ok(RootView {
                schema_version: CURRENT_SCHEMA_VERSION,
                epoch: 0,
                collection_uuids: Vec::new(),
            }),
        }
    }

    fn collection_view(&self, collection_uuid: &str) -> Result<CollectionDocView> {
        let key = DocumentKey::collection(collection_uuid);
        match self.store.get(&key) {
            Some(state) => extract_collection_doc_view(&state.automerge_doc, collection_uuid),
            None => Ok(CollectionDocView {
                schema_version: CURRENT_SCHEMA_VERSION,
                meta: remi_things_crdt::CollectionMetaView {
                    id: collection_uuid.to_string(),
                    title: String::new(),
                    status: "active".to_string(),
                    edit_clock: remi_things_crdt::view::EditClock::zero(),
                    tombstone: None,
                    trigger: None,
                    attrs: None,
                },
                things: Vec::new(),
            }),
        }
    }

    fn thing_markdown_view(&self, thing_uuid: &str) -> Result<ThingMarkdownView> {
        let key = DocumentKey::thing_content(thing_uuid);
        match self.store.get(&key) {
            Some(state) => extract_thing_markdown_view(&state.automerge_doc, thing_uuid),
            None => Ok(ThingMarkdownView {
                schema_version: CURRENT_SCHEMA_VERSION,
                thing_uuid: thing_uuid.to_string(),
                content: None,
            }),
        }
    }

    fn thing_content_view(
        &self,
        document_uuid: &str,
        thing_uuid: &str,
    ) -> Result<Option<ThingContentView>> {
        let key = DocumentKey::thing_content(document_uuid);
        let Some(state) = self.store.get(&key) else {
            return Ok(None);
        };

        Ok(Some(extract_thing_content_view(
            &state.automerge_doc,
            document_uuid,
            thing_uuid,
        )?))
    }

    fn thing_content_document_uuids(&self, thing_uuid: &str) -> Result<Vec<String>> {
        let mut uuids = Vec::new();

        for (key, state) in self.store.iter() {
            if key.data_type != CrdtDataType::ThingMarkdown {
                continue;
            }

            let Ok(view) = extract_thing_content_view(&state.automerge_doc, &key.uuid, thing_uuid)
            else {
                continue;
            };

            if view.thing_uuid == thing_uuid {
                uuids.push(key.uuid.clone());
            }
        }

        uuids.sort();
        Ok(uuids)
    }

    fn get_content_entries(
        &self,
        collection_uuid: &str,
        thing_uuid: &str,
    ) -> Result<Vec<ContentEntry>> {
        let key = DocumentKey::collection(collection_uuid);
        let state = self.store.get(&key).context("Collection not found")?;

        let view = extract_collection_doc_view(&state.automerge_doc, collection_uuid)?;
        let collection_deleted = view
            .meta
            .tombstone
            .as_ref()
            .map(|t| t.deleted)
            .unwrap_or(false);
        if collection_deleted {
            anyhow::bail!("Thing not found");
        }

        let thing = view
            .things
            .iter()
            .find(|thing| thing.id == thing_uuid)
            .context("Thing not found")?;

        let thing_deleted = thing.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false);
        if thing_deleted {
            anyhow::bail!("Thing not found");
        }

        Ok(thing.built_in.content_entries.clone())
    }

    fn find_thing_collection_uuid(&self, thing_uuid: &str) -> Option<String> {
        for (key, state) in self.store.iter() {
            if key.data_type != CrdtDataType::Collection {
                continue;
            }

            let view = match extract_collection_doc_view(&state.automerge_doc, &key.uuid) {
                Ok(view) => view,
                Err(_) => continue,
            };

            if view
                .meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false)
            {
                continue;
            }

            for thing in &view.things {
                let thing_deleted = thing.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false);
                if !thing_deleted && thing.id == thing_uuid {
                    return Some(key.uuid.clone());
                }
            }
        }

        None
    }

    fn collection_uuids_from_documents(&self) -> Result<Vec<String>> {
        let mut collection_uuids = Vec::new();

        for (key, state) in self.store.iter() {
            if key.data_type != CrdtDataType::Collection {
                continue;
            }

            let view = extract_collection_doc_view(&state.automerge_doc, &key.uuid)?;
            if view
                .meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false)
            {
                continue;
            }

            collection_uuids.push(key.uuid.clone());
        }

        collection_uuids.sort();
        Ok(collection_uuids)
    }

    fn active_collection_uuids(&self) -> Result<HashSet<String>> {
        let mut live = HashSet::new();

        for coll_uuid in self.collection_uuids_from_documents()? {
            live.insert(coll_uuid);
        }

        Ok(live)
    }

    fn active_thing_uuids(&self) -> Result<HashSet<String>> {
        let live_collections = self.active_collection_uuids()?;
        let mut live_things = HashSet::new();

        for coll_uuid in &live_collections {
            let key = DocumentKey::collection(coll_uuid);
            let Some(state) = self.store.get(&key) else {
                continue;
            };

            let view = extract_collection_doc_view(&state.automerge_doc, coll_uuid)?;
            if view
                .meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false)
            {
                continue;
            }

            for thing in &view.things {
                if !thing.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false) {
                    live_things.insert(thing.id.clone());
                }
            }
        }

        Ok(live_things)
    }

    fn active_content_document_uuids(&self) -> Result<HashSet<String>> {
        let live_collections = self.active_collection_uuids()?;
        let mut live_docs = HashSet::new();

        for coll_uuid in &live_collections {
            let key = DocumentKey::collection(coll_uuid);
            let Some(state) = self.store.get(&key) else {
                continue;
            };

            let view = extract_collection_doc_view(&state.automerge_doc, coll_uuid)?;
            if view
                .meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false)
            {
                continue;
            }

            for thing in &view.things {
                if thing.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false) {
                    continue;
                }

                for entry in self.get_content_entries(coll_uuid, &thing.id)? {
                    match entry.payload {
                        ContentEntryPayload::Markdown { doc_uuid } => {
                            live_docs.insert(doc_uuid);
                        }
                        ContentEntryPayload::JsonObject(field) => {
                            live_docs.insert(field.data_doc_uuid);
                            if let Some(schema_doc_uuid) = field.schema_doc_uuid {
                                live_docs.insert(schema_doc_uuid);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(live_docs)
    }

    fn extract_snapshot(&self) -> Result<ThingsSnapshot> {
        self.extract_snapshot_with_options(SnapshotOptions::default())
    }

    fn extract_snapshot_with_options(&self, options: SnapshotOptions) -> Result<ThingsSnapshot> {
        let mut collections = Vec::new();
        let mut things = Vec::new();
        let content_registry = ContentTypeRegistry::new();

        let mut collection_views = Vec::new();
        for (key, state) in self.store.iter() {
            if key.data_type != CrdtDataType::Collection {
                continue;
            }

            let coll_view = extract_collection_doc_view(&state.automerge_doc, &key.uuid)?;
            let deleted = coll_view
                .meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false);
            if deleted {
                continue;
            }

            collection_views.push((key.uuid.clone(), coll_view));
        }
        collection_views.sort_by(|(left, _), (right, _)| left.cmp(right));

        for (coll_uuid, coll_view) in collection_views {
            let deleted = coll_view
                .meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false);
            if deleted {
                continue;
            }

            let trigger_uuid = desired_trigger_uuid(deleted, &coll_view.meta.trigger);
            let collection_timestamps = extract_entity_timestamps(coll_view.meta.attrs.as_ref());
            collections.push(ThingCollectionEntry {
                uuid: coll_view.meta.id.clone(),
                title: coll_view.meta.title.clone(),
                trigger_uuid,
                card_jsx: extract_collection_card_jsx(coll_view.meta.attrs.as_ref()),
                created_at: collection_timestamps.created_at.unwrap_or_default(),
                updated_at: collection_timestamps.updated_at.unwrap_or_default(),
                actor_type: None,
                actor_app_id: None,
                actor_display_name: None,
            });

            for thing_meta in &coll_view.things {
                let thing_deleted = thing_meta
                    .tombstone
                    .as_ref()
                    .map(|t| t.deleted)
                    .unwrap_or(false);
                if thing_deleted {
                    continue;
                }

                let thing_trigger = desired_trigger_uuid(thing_deleted, &thing_meta.trigger);
                let thing_timestamps = extract_entity_timestamps(thing_meta.attrs.as_ref());
                let content = if options.include_content {
                    let md_view = self.thing_markdown_view(&thing_meta.id)?;
                    md_view.content
                } else {
                    None
                };
                let data = content_registry.serialize_thing_snapshot_data(
                    thing_meta,
                    content.as_ref(),
                    options,
                );

                things.push(ThingEntry {
                    uuid: thing_meta.id.clone(),
                    title: thing_meta.title.clone().unwrap_or_default(),
                    datatype: thing_meta.datatype.clone(),
                    data,
                    collection_uuid: coll_uuid.clone(),
                    trigger_uuid: thing_trigger,
                    parent_uuid: thing_meta.parent_id.clone(),
                    created_at: thing_timestamps.created_at.unwrap_or_default(),
                    updated_at: thing_timestamps.updated_at.unwrap_or_default(),
                    status: thing_meta.status.as_storage_str().to_string(),
                    status_timestamp_ms: thing_meta.status.timestamp_ms(),
                    actor_type: None,
                    actor_app_id: None,
                    actor_display_name: None,
                });
            }
        }

        Ok(ThingsSnapshot {
            collections,
            things,
        })
    }
}

struct ThingsDomainWriter<'a> {
    device_id: &'a str,
    documents: &'a mut HashMap<DocumentKey, DocumentState>,
}

impl<'a> ThingsDomainWriter<'a> {
    fn new(device_id: &'a str, documents: &'a mut HashMap<DocumentKey, DocumentState>) -> Self {
        Self {
            device_id,
            documents,
        }
    }

    fn new_document_state(&self, automerge_doc: Vec<u8>) -> DocumentState {
        DocumentState {
            automerge_doc,
            sync_state: Vec::new(),
            dirty: true,
            last_sync_at: None,
        }
    }

    fn init_root(&mut self) -> Result<()> {
        let key = DocumentKey::root();
        if self.documents.contains_key(&key) {
            return Ok(());
        }

        let doc_bytes = Schema::init_root_doc(self.device_id)?;
        self.documents
            .insert(key, self.new_document_state(doc_bytes));
        Ok(())
    }

    fn get_or_init_collection(&mut self, collection_uuid: &str) -> Result<()> {
        let key = DocumentKey::collection(collection_uuid);
        if !self.documents.contains_key(&key) {
            let doc_bytes = Schema::init_collection_doc(self.device_id, collection_uuid)?;
            self.documents
                .insert(key, self.new_document_state(doc_bytes));
            self.add_collection(collection_uuid)?;
        }
        Ok(())
    }

    fn get_or_init_thing_content(
        &mut self,
        document_uuid: &str,
        thing_uuid: &str,
        content_type: &str,
    ) -> Result<()> {
        let key = DocumentKey::thing_content(document_uuid);
        if !self.documents.contains_key(&key) {
            let doc_bytes = Schema::init_thing_content_doc(
                self.device_id,
                document_uuid,
                thing_uuid,
                content_type,
            )?;
            self.documents
                .insert(key, self.new_document_state(doc_bytes));
        }
        Ok(())
    }

    fn apply_document_update<F>(&mut self, key: &DocumentKey, update: F) -> Result<()>
    where
        F: FnOnce(&[u8]) -> Result<Vec<u8>>,
    {
        let current_doc = self
            .documents
            .get(key)
            .map(|state| state.automerge_doc.clone())
            .ok_or_else(|| anyhow::anyhow!("Missing document for key {:?}", key))?;
        let updated_doc = update(&current_doc)?;
        let state = self
            .documents
            .get_mut(key)
            .ok_or_else(|| anyhow::anyhow!("Missing document for key {:?}", key))?;
        state.automerge_doc = updated_doc;
        state.dirty = true;
        Ok(())
    }

    fn apply_collection_update(&mut self, collection_uuid: &str, op: CollectionOp) -> Result<()> {
        self.get_or_init_collection(collection_uuid)?;
        let key = DocumentKey::collection(collection_uuid);
        self.apply_document_update(&key, |doc| {
            apply_collection_op(doc, self.device_id, collection_uuid, op)
        })
    }

    fn apply_thing_content_update(
        &mut self,
        document_uuid: &str,
        thing_uuid: &str,
        content_type: &str,
        op: ThingMarkdownOp,
    ) -> Result<()> {
        self.get_or_init_thing_content(document_uuid, thing_uuid, content_type)?;
        let key = DocumentKey::thing_content(document_uuid);
        self.apply_document_update(&key, |doc| {
            apply_thing_markdown_op(doc, self.device_id, thing_uuid, op)
        })
    }

    fn apply_thing_markdown_update(&mut self, thing_uuid: &str, op: ThingMarkdownOp) -> Result<()> {
        self.apply_thing_content_update(thing_uuid, thing_uuid, "markdown", op)
    }

    fn root_collection_list_ids(doc: &AutoCommit) -> Result<Vec<ObjId>> {
        Ok(doc
            .get_all(automerge::ROOT, Schema::KEY_COLLECTION_UUIDS)?
            .into_iter()
            .filter_map(|(value, obj_id)| match value {
                AmValue::Object(ObjType::List) => Some(obj_id),
                _ => None,
            })
            .collect())
    }

    fn root_list_contains_collection(
        doc: &AutoCommit,
        list_obj: &ObjId,
        collection_uuid: &str,
    ) -> Result<bool> {
        for index in 0..doc.length(list_obj) {
            if let Some((AmValue::Scalar(value), _)) = doc.get(list_obj, index)? {
                if let ScalarValue::Str(existing_uuid) = value.as_ref() {
                    if existing_uuid == collection_uuid {
                        return Ok(true);
                    }
                }
            }
        }

        Ok(false)
    }

    fn remove_collection_from_root_lists(
        doc: &mut AutoCommit,
        collection_uuid: &str,
    ) -> Result<()> {
        for list_obj in Self::root_collection_list_ids(doc)? {
            let mut indexes_to_delete = Vec::new();

            for index in 0..doc.length(&list_obj) {
                if let Some((AmValue::Scalar(value), _)) = doc.get(&list_obj, index)? {
                    if let ScalarValue::Str(existing_uuid) = value.as_ref() {
                        if existing_uuid == collection_uuid {
                            indexes_to_delete.push(index);
                        }
                    }
                }
            }

            for index in indexes_to_delete.into_iter().rev() {
                doc.delete(&list_obj, index)
                    .context("Failed to remove collection uuid from root list")?;
            }
        }

        Ok(())
    }

    fn update_root_collection_membership(
        device_id: &str,
        doc_bytes: &[u8],
        collection_uuid: &str,
        should_exist: bool,
    ) -> Result<Vec<u8>> {
        let mut doc = if doc_bytes.is_empty() {
            let init_bytes = Schema::init_root_doc(device_id)?;
            AutoCommit::load(&init_bytes).context("Failed to load init root doc")?
        } else {
            AutoCommit::load(doc_bytes).context("Failed to load root doc")?
        };
        doc.set_actor(ActorId::from(device_id.as_bytes().to_vec()));

        let list_objs = Self::root_collection_list_ids(&doc)?;

        if should_exist {
            let mut already_present = false;
            for list_obj in &list_objs {
                if Self::root_list_contains_collection(&doc, list_obj, collection_uuid)? {
                    already_present = true;
                    break;
                }
            }

            if !already_present {
                let target_list = if let Some(existing) = list_objs.first() {
                    existing.clone()
                } else {
                    doc.put_object(automerge::ROOT, Schema::KEY_COLLECTION_UUIDS, ObjType::List)
                        .context("Failed to create collection_uuids list")?
                };
                doc.insert(
                    &target_list,
                    doc.length(&target_list),
                    collection_uuid.to_string(),
                )
                .context("Failed to add collection uuid to root list")?;
            }
        } else {
            Self::remove_collection_from_root_lists(&mut doc, collection_uuid)?;
        }

        Ok(doc.save())
    }

    fn add_collection(&mut self, collection_uuid: &str) -> Result<()> {
        self.init_root()?;
        let key = DocumentKey::root();
        let device_id = self.device_id;
        self.apply_document_update(&key, |doc| {
            Self::update_root_collection_membership(device_id, doc, collection_uuid, true)
        })
    }

    fn remove_collection(&mut self, collection_uuid: &str) -> Result<()> {
        self.init_root()?;
        let key = DocumentKey::root();
        let device_id = self.device_id;
        self.apply_document_update(&key, |doc| {
            Self::update_root_collection_membership(device_id, doc, collection_uuid, false)
        })
    }

    fn update_collection_meta(
        &mut self,
        collection_uuid: &str,
        title: Option<String>,
        status: Option<String>,
        trigger: TriggerUpdate,
        attrs_json: Option<String>,
    ) -> Result<()> {
        self.apply_collection_update(
            collection_uuid,
            CollectionOp::UpdateMeta {
                title,
                status,
                trigger,
                attrs_json,
            },
        )
    }

    fn ensure_live_collection_exists(&self, collection_uuid: &str) -> Result<()> {
        let key = DocumentKey::collection(collection_uuid);
        let Some(state) = self.documents.get(&key) else {
            anyhow::bail!(
                "collection '{}' must exist before adding or reparenting things",
                collection_uuid
            );
        };

        let view = extract_collection_doc_view(&state.automerge_doc, collection_uuid)?;
        if view
            .meta
            .tombstone
            .as_ref()
            .map(|t| t.deleted)
            .unwrap_or(false)
        {
            anyhow::bail!("collection '{}' is deleted", collection_uuid);
        }

        Ok(())
    }

    fn upsert_thing_meta(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        datatype: Option<ThingDatatype>,
        status: Option<String>,
        title: Option<String>,
        parent_uuid: Option<String>,
        trigger: TriggerUpdate,
        attrs_json: Option<String>,
    ) -> Result<()> {
        self.ensure_live_collection_exists(collection_uuid)?;
        self.apply_collection_update(
            collection_uuid,
            CollectionOp::UpsertThingMeta {
                thing_id: thing_uuid.to_string(),
                datatype,
                status,
                status_timestamp_ms: None,
                title,
                parent_id: parent_uuid,
                trigger,
                built_in: None,
                attrs_json,
            },
        )
    }

    fn delete_collection(&mut self, collection_uuid: &str) -> Result<()> {
        self.apply_collection_update(collection_uuid, CollectionOp::Delete)?;
        self.remove_collection(collection_uuid)
    }

    fn delete_thing(&mut self, collection_uuid: &str, thing_uuid: &str) -> Result<()> {
        if !self
            .documents
            .contains_key(&DocumentKey::collection(collection_uuid))
        {
            return Ok(());
        }

        self.apply_collection_update(
            collection_uuid,
            CollectionOp::DeleteThing {
                thing_id: thing_uuid.to_string(),
            },
        )
    }

    fn add_content_entry(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        entry: ContentEntry,
    ) -> Result<()> {
        let built_in = ThingBuiltInFieldsUpdate {
            add_entries: vec![entry],
            ..Default::default()
        };

        self.apply_collection_update(
            collection_uuid,
            CollectionOp::UpsertThingMeta {
                thing_id: thing_uuid.to_string(),
                datatype: None,
                status: None,
                status_timestamp_ms: None,
                title: None,
                parent_id: None,
                trigger: TriggerUpdate::Noop,
                built_in: Some(built_in),
                attrs_json: None,
            },
        )
    }

    fn update_content_entry(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        entry_id: &str,
        title: Option<Option<String>>,
        order: Option<f64>,
        payload: Option<ContentEntryPayload>,
    ) -> Result<()> {
        let entry_update = remi_things_crdt::ContentEntryUpdate {
            id: entry_id.to_string(),
            title,
            order,
            payload,
        };

        let built_in = ThingBuiltInFieldsUpdate {
            update_entries: vec![entry_update],
            ..Default::default()
        };

        self.apply_collection_update(
            collection_uuid,
            CollectionOp::UpsertThingMeta {
                thing_id: thing_uuid.to_string(),
                datatype: None,
                status: None,
                status_timestamp_ms: None,
                title: None,
                parent_id: None,
                trigger: TriggerUpdate::Noop,
                built_in: Some(built_in),
                attrs_json: None,
            },
        )
    }

    fn delete_content_entry(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        entry_id: &str,
    ) -> Result<()> {
        let built_in = ThingBuiltInFieldsUpdate {
            delete_entry_ids: vec![entry_id.to_string()],
            ..Default::default()
        };

        self.apply_collection_update(
            collection_uuid,
            CollectionOp::UpsertThingMeta {
                thing_id: thing_uuid.to_string(),
                datatype: None,
                status: None,
                status_timestamp_ms: None,
                title: None,
                parent_id: None,
                trigger: TriggerUpdate::Noop,
                built_in: Some(built_in),
                attrs_json: None,
            },
        )
    }

    fn set_thing_content(&mut self, thing_uuid: &str, content: Content) -> Result<()> {
        self.apply_thing_markdown_update(thing_uuid, ThingMarkdownOp::SetContent { content })
    }

    fn set_thing_content_document(
        &mut self,
        document_uuid: &str,
        thing_uuid: &str,
        content_type: &str,
        content: Content,
    ) -> Result<()> {
        self.apply_thing_content_update(
            document_uuid,
            thing_uuid,
            content_type,
            ThingMarkdownOp::SetContent { content },
        )
    }

    fn splice_thing_text(
        &mut self,
        thing_uuid: &str,
        block_id: &str,
        index: usize,
        delete: usize,
        insert: &str,
    ) -> Result<()> {
        self.apply_thing_markdown_update(
            thing_uuid,
            ThingMarkdownOp::SpliceText {
                block_id: block_id.to_string(),
                index,
                delete,
                insert: insert.to_string(),
            },
        )
    }
}

/// Manages the set of CRDT documents (Root, Collections, ThingMarkdown)
#[derive(Debug, Clone)]
pub struct ThingsDocumentSet {
    device_id: String,
    documents: HashMap<DocumentKey, DocumentState>,
}

impl ThingsDocumentSet {
    fn store_view(&self) -> DocumentStoreView<'_> {
        DocumentStoreView::new(&self.documents)
    }

    fn store_mut(&mut self) -> DocumentStoreMut<'_> {
        DocumentStoreMut::new(&self.device_id, &mut self.documents)
    }

    fn domain_reader(&self) -> ThingsDomainReader<'_> {
        ThingsDomainReader::new(self.store_view())
    }

    fn domain_writer(&mut self) -> ThingsDomainWriter<'_> {
        ThingsDomainWriter::new(&self.device_id, &mut self.documents)
    }

    /// Create a new empty document set
    pub fn new(device_id: &str) -> Self {
        Self {
            device_id: device_id.to_string(),
            documents: HashMap::new(),
        }
    }

    /// Initialize with a root document
    pub fn init_root(&mut self) -> Result<()> {
        self.domain_writer().init_root()
    }

    pub fn has_root_document(&self) -> bool {
        self.documents.contains_key(&DocumentKey::root())
    }

    /// Get a document by key
    pub fn get(&self, key: &DocumentKey) -> Option<&DocumentState> {
        self.documents.get(key)
    }

    /// Get a document mutably
    pub fn get_mut(&mut self, key: &DocumentKey) -> Option<&mut DocumentState> {
        self.documents.get_mut(key)
    }

    /// Insert or update a document
    pub fn set(&mut self, key: DocumentKey, state: DocumentState) {
        self.store_mut().set(key, state);
    }

    /// Check if a document exists
    pub fn contains(&self, key: &DocumentKey) -> bool {
        self.documents.contains_key(key)
    }

    /// Get all dirty documents, ordered by sync priority
    fn dirty_documents(&self) -> Vec<(&DocumentKey, &DocumentState)> {
        let mut dirty: Vec<_> = self
            .documents
            .iter()
            .filter(|(_, state)| state.dirty)
            .collect();
        dirty.sort_by_key(|(key, _)| key.data_type.sync_priority());
        dirty
    }

    /// Best-effort local index maintenance.
    ///
    /// Rebuild the root discovery index from live collection documents without
    /// changing business visibility. This is intended for manual maintenance or
    /// diagnostics only; normal reads and sync must not depend on it.
    ///
    /// Returns the number of collections that were added back to the root index.
    pub fn maintain_root_index_from_live_collections(&mut self) -> Result<usize> {
        let root_view = self.root_view()?;
        let root_uuids: std::collections::HashSet<&str> = root_view
            .collection_uuids
            .iter()
            .map(|s| s.as_str())
            .collect();

        let orphaned_colls: Vec<String> = self
            .documents
            .keys()
            .filter(|k| k.data_type == CrdtDataType::Collection && !root_uuids.contains(k.uuid.as_str()))
            .filter_map(|k| {
                match self.collection_view(&k.uuid) {
                    Ok(view)
                        if !view
                            .meta
                            .tombstone
                            .as_ref()
                            .map(|t| t.deleted)
                            .unwrap_or(false) =>
                    {
                        Some(k.uuid.clone())
                    }
                    Ok(_) => {
                        tracing::debug!(
                            collection_uuid = k.uuid.as_str(),
                            "maintain_root_index_from_live_collections: skipping tombstoned collection"
                        );
                        None
                    }
                    Err(err) => {
                        tracing::warn!(
                            collection_uuid = k.uuid.as_str(),
                            error = %err,
                            "maintain_root_index_from_live_collections: failed to inspect collection, skipping relink"
                        );
                        None
                    }
                }
            })
            .collect();

        if orphaned_colls.is_empty() {
            return Ok(0);
        }

        for coll_uuid in &orphaned_colls {
            tracing::warn!(
                collection_uuid = coll_uuid.as_str(),
                "maintain_root_index_from_live_collections: re-linking orphaned collection to root index"
            );
            self.add_collection(coll_uuid)?;
        }

        Ok(orphaned_colls.len())
    }

    /// Return collection UUIDs that are still live according to collection documents.
    pub fn active_collection_uuids(&self) -> Result<HashSet<String>> {
        self.domain_reader().active_collection_uuids()
    }

    /// Return thing UUIDs that are still reachable through live collection documents.
    pub fn active_thing_uuids(&self) -> Result<HashSet<String>> {
        self.domain_reader().active_thing_uuids()
    }

    pub fn active_content_document_uuids(&self) -> Result<HashSet<String>> {
        self.domain_reader().active_content_document_uuids()
    }

    // ===== V3 Compaction =====

    /// Try to compact a document if it exceeds the size threshold.
    /// Returns true if compaction was performed.
    pub fn maybe_compact(&mut self, key: &DocumentKey) -> Result<bool> {
        self.maybe_compact_with_threshold(key, DEFAULT_COMPACTION_THRESHOLD)
    }

    /// Try to compact a document if it exceeds the specified threshold.
    /// Returns true if compaction was performed.
    pub fn maybe_compact_with_threshold(
        &mut self,
        key: &DocumentKey,
        threshold: usize,
    ) -> Result<bool> {
        self.store_mut()
            .maybe_compact_with_threshold(key, threshold)
    }

    /// Compact all documents that exceed the threshold.
    /// Returns the number of documents compacted.
    pub fn compact_all(&mut self) -> Result<usize> {
        self.compact_all_with_threshold(DEFAULT_COMPACTION_THRESHOLD)
    }

    /// Compact all documents that exceed the specified threshold.
    /// Returns the number of documents compacted.
    pub fn compact_all_with_threshold(&mut self, threshold: usize) -> Result<usize> {
        self.store_mut().compact_all_with_threshold(threshold)
    }

    // ===== Root Operations =====

    /// Add a collection to the root document
    pub fn add_collection(&mut self, collection_uuid: &str) -> Result<()> {
        self.domain_writer().add_collection(collection_uuid)
    }

    /// Remove a collection from the root document
    pub fn remove_collection(&mut self, collection_uuid: &str) -> Result<()> {
        self.domain_writer().remove_collection(collection_uuid)
    }

    /// Get root view
    pub fn root_view(&self) -> Result<RootView> {
        self.domain_reader().root_view()
    }

    // ===== Collection Operations =====

    /// Get or create a collection document
    pub fn get_or_init_collection(&mut self, collection_uuid: &str) -> Result<&DocumentState> {
        self.domain_writer()
            .get_or_init_collection(collection_uuid)?;
        let key = DocumentKey::collection(collection_uuid);
        Ok(self.documents.get(&key).unwrap())
    }

    /// Update collection metadata
    pub fn update_collection_meta(
        &mut self,
        collection_uuid: &str,
        title: Option<String>,
        status: Option<String>,
        trigger: TriggerUpdate,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        self.update_collection_meta_with_timestamps(
            collection_uuid,
            title,
            status,
            trigger,
            None,
            None,
        )
    }

    pub fn update_collection_meta_with_timestamps(
        &mut self,
        collection_uuid: &str,
        title: Option<String>,
        status: Option<String>,
        trigger: TriggerUpdate,
        created_at: Option<String>,
        updated_at: Option<String>,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        let existed = self.collection_is_live(collection_uuid)?;
        let existing_attrs = if existed {
            Some(self.collection_view(collection_uuid)?.meta.attrs)
        } else {
            None
        };
        let timestamps = resolve_entity_timestamps(
            existing_attrs.as_ref().and_then(|attrs| attrs.as_ref()),
            created_at,
            updated_at,
            existed,
        );
        let attrs_json = merge_entity_timestamps_into_attrs(
            existing_attrs.as_ref().and_then(|attrs| attrs.as_ref()),
            timestamps.created_at,
            timestamps.updated_at,
        )?;
        self.domain_writer().update_collection_meta(
            collection_uuid,
            title,
            status,
            trigger,
            Some(attrs_json),
        )?;

        let mut events = Vec::new();
        if !existed {
            events.push(ThingsDocumentEvent::root(ThingsDocumentChangeKind::Updated));
        }
        events.push(ThingsDocumentEvent::collection(
            if existed {
                ThingsDocumentChangeKind::Updated
            } else {
                ThingsDocumentChangeKind::Created
            },
            collection_uuid,
        ));
        Ok(events)
    }

    pub fn update_collection_attrs(
        &mut self,
        collection_uuid: &str,
        attrs: Option<Value>,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        let existed = self.collection_is_live(collection_uuid)?;
        if !existed {
            anyhow::bail!("Collection not found: {collection_uuid}");
        }

        let existing_attrs = self.collection_view(collection_uuid)?.meta.attrs;
        let timestamps = resolve_entity_timestamps(existing_attrs.as_ref(), None, None, true);
        let normalized_attrs = normalize_entity_attrs_value(attrs);
        let attrs_json = merge_entity_timestamps_into_attrs(
            normalized_attrs.as_ref(),
            timestamps.created_at,
            timestamps.updated_at,
        )?;

        self.domain_writer().update_collection_meta(
            collection_uuid,
            None,
            None,
            TriggerUpdate::Noop,
            Some(attrs_json),
        )?;

        Ok(vec![ThingsDocumentEvent::collection(
            ThingsDocumentChangeKind::Updated,
            collection_uuid,
        )])
    }

    /// Upsert a thing in a collection
    pub fn upsert_thing_meta(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        datatype: Option<ThingDatatype>,
        status: Option<String>,
        title: Option<String>,
        parent_uuid: Option<String>,
        trigger: TriggerUpdate,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        self.upsert_thing_meta_with_timestamps(
            collection_uuid,
            thing_uuid,
            datatype,
            status,
            title,
            parent_uuid,
            trigger,
            None,
            None,
        )
    }

    pub fn upsert_thing_meta_with_timestamps(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        datatype: Option<ThingDatatype>,
        status: Option<String>,
        title: Option<String>,
        parent_uuid: Option<String>,
        trigger: TriggerUpdate,
        created_at: Option<String>,
        updated_at: Option<String>,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        let existed = self.thing_is_live_in_collection(collection_uuid, thing_uuid)?;
        let existing_attrs = if existed {
            self.collection_view(collection_uuid)?
                .things
                .into_iter()
                .find(|thing| thing.id == thing_uuid)
                .and_then(|thing| thing.attrs)
        } else {
            None
        };
        let timestamps =
            resolve_entity_timestamps(existing_attrs.as_ref(), created_at, updated_at, existed);
        let attrs_json = merge_entity_timestamps_into_attrs(
            existing_attrs.as_ref(),
            timestamps.created_at,
            timestamps.updated_at,
        )?;
        self.domain_writer().upsert_thing_meta(
            collection_uuid,
            thing_uuid,
            datatype,
            status,
            title,
            parent_uuid,
            trigger,
            Some(attrs_json),
        )?;

        Ok(vec![ThingsDocumentEvent::thing(
            if existed {
                ThingsDocumentChangeKind::Updated
            } else {
                ThingsDocumentChangeKind::Created
            },
            collection_uuid,
            thing_uuid,
        )])
    }

    pub fn update_thing_attrs(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        attrs: Option<Value>,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        let existed = self.thing_is_live_in_collection(collection_uuid, thing_uuid)?;
        if !existed {
            anyhow::bail!("Thing not found: {thing_uuid}");
        }

        let existing_attrs = self
            .collection_view(collection_uuid)?
            .things
            .into_iter()
            .find(|thing| thing.id == thing_uuid)
            .and_then(|thing| thing.attrs);
        let timestamps = resolve_entity_timestamps(existing_attrs.as_ref(), None, None, true);
        let normalized_attrs = normalize_entity_attrs_value(attrs);
        let attrs_json = merge_entity_timestamps_into_attrs(
            normalized_attrs.as_ref(),
            timestamps.created_at,
            timestamps.updated_at,
        )?;

        self.domain_writer().upsert_thing_meta(
            collection_uuid,
            thing_uuid,
            None,
            None,
            None,
            None,
            TriggerUpdate::Noop,
            Some(attrs_json),
        )?;

        Ok(vec![ThingsDocumentEvent::thing(
            ThingsDocumentChangeKind::Updated,
            collection_uuid,
            thing_uuid,
        )])
    }

    /// Delete a collection by tombstoning its collection document and removing
    /// the root reference. Child things become unreachable through the deleted
    /// collection and are pruned from snapshots without deleting their docs.
    pub fn delete_collection(&mut self, collection_uuid: &str) -> Result<Vec<ThingsDocumentEvent>> {
        if !self.collection_is_live(collection_uuid)? {
            return Ok(Vec::new());
        }

        self.domain_writer().delete_collection(collection_uuid)?;
        Ok(vec![
            ThingsDocumentEvent::collection(ThingsDocumentChangeKind::Deleted, collection_uuid),
            ThingsDocumentEvent::root(ThingsDocumentChangeKind::Updated),
        ])
    }

    /// Delete a thing from a collection
    pub fn delete_thing(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        if !self.thing_is_live_in_collection(collection_uuid, thing_uuid)? {
            return Ok(Vec::new());
        }

        self.domain_writer()
            .delete_thing(collection_uuid, thing_uuid)?;
        Ok(vec![ThingsDocumentEvent::thing(
            ThingsDocumentChangeKind::Deleted,
            collection_uuid,
            thing_uuid,
        )])
    }

    /// Add a content entry to a thing (V3 multi-value)
    pub fn add_content_entry(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        entry: ContentEntry,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        let entry_id = entry.id.clone();
        let existed = self.content_entry_exists(collection_uuid, thing_uuid, &entry_id)?;
        self.domain_writer()
            .add_content_entry(collection_uuid, thing_uuid, entry)?;
        Ok(vec![ThingsDocumentEvent::content_entry(
            if existed {
                ThingsDocumentChangeKind::Updated
            } else {
                ThingsDocumentChangeKind::Created
            },
            collection_uuid,
            thing_uuid,
            &entry_id,
        )])
    }

    /// Update a content entry on a thing (V3 multi-value)
    pub fn update_content_entry(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        entry_id: &str,
        title: Option<Option<String>>,
        order: Option<f64>,
        payload: Option<ContentEntryPayload>,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        let existed = self.content_entry_exists(collection_uuid, thing_uuid, entry_id)?;
        self.domain_writer().update_content_entry(
            collection_uuid,
            thing_uuid,
            entry_id,
            title,
            order,
            payload,
        )?;
        Ok(vec![ThingsDocumentEvent::content_entry(
            if existed {
                ThingsDocumentChangeKind::Updated
            } else {
                ThingsDocumentChangeKind::Created
            },
            collection_uuid,
            thing_uuid,
            entry_id,
        )])
    }

    /// Delete a content entry from a thing (V3 multi-value)
    pub fn delete_content_entry(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        entry_id: &str,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        if !self.content_entry_exists(collection_uuid, thing_uuid, entry_id)? {
            return Ok(Vec::new());
        }

        self.domain_writer()
            .delete_content_entry(collection_uuid, thing_uuid, entry_id)?;
        Ok(vec![ThingsDocumentEvent::content_entry(
            ThingsDocumentChangeKind::Deleted,
            collection_uuid,
            thing_uuid,
            entry_id,
        )])
    }

    /// Get content entries for a thing
    pub fn get_content_entries(
        &self,
        collection_uuid: &str,
        thing_uuid: &str,
    ) -> Result<Vec<ContentEntry>> {
        self.domain_reader()
            .get_content_entries(collection_uuid, thing_uuid)
    }

    /// Find which collection a thing belongs to by scanning all collection documents.
    ///
    /// This is more robust than `extract_snapshot()` because it does **not** depend on
    /// the root document listing the collection. It's useful when the root <-> collection
    /// linkage might be stale (e.g. after sync or migration).
    pub fn find_thing_collection_uuid(&self, thing_uuid: &str) -> Option<String> {
        self.domain_reader().find_thing_collection_uuid(thing_uuid)
    }

    /// Get collection view
    pub fn collection_view(&self, collection_uuid: &str) -> Result<CollectionDocView> {
        self.domain_reader().collection_view(collection_uuid)
    }

    fn collection_is_live(&self, collection_uuid: &str) -> Result<bool> {
        let key = DocumentKey::collection(collection_uuid);
        let Some(state) = self.documents.get(&key) else {
            return Ok(false);
        };
        let view = extract_collection_doc_view(&state.automerge_doc, collection_uuid)?;
        Ok(!view
            .meta
            .tombstone
            .as_ref()
            .map(|t| t.deleted)
            .unwrap_or(false))
    }

    fn thing_is_live_in_collection(&self, collection_uuid: &str, thing_uuid: &str) -> Result<bool> {
        let view = self.collection_view(collection_uuid)?;
        if view
            .meta
            .tombstone
            .as_ref()
            .map(|t| t.deleted)
            .unwrap_or(false)
        {
            return Ok(false);
        }

        Ok(view.things.iter().any(|thing| {
            thing.id == thing_uuid && !thing.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false)
        }))
    }

    fn content_entry_exists(
        &self,
        collection_uuid: &str,
        thing_uuid: &str,
        entry_id: &str,
    ) -> Result<bool> {
        Ok(self
            .get_content_entries(collection_uuid, thing_uuid)?
            .iter()
            .any(|entry| entry.id == entry_id))
    }

    fn thing_markdown_document_exists(&self, thing_uuid: &str) -> bool {
        self.documents
            .contains_key(&DocumentKey::thing_content(thing_uuid))
    }

    // ===== ThingMarkdown Operations =====

    /// Set content on a thing markdown document from a typed payload.
    pub fn set_thing_content_from_payload(
        &mut self,
        thing_uuid: &str,
        datatype: &ThingDatatype,
        payload: &Value,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        let content = ContentTypeRegistry::new().markdown_content_from_value(datatype, payload);
        let existed = self.thing_markdown_document_exists(thing_uuid);
        self.domain_writer()
            .set_thing_content(thing_uuid, content)?;
        Ok(vec![ThingsDocumentEvent::thing_markdown(
            if existed {
                ThingsDocumentChangeKind::Updated
            } else {
                ThingsDocumentChangeKind::Created
            },
            self.find_thing_collection_uuid(thing_uuid).as_deref(),
            thing_uuid,
        )])
    }

    /// Reconcile built-in content entries from a snapshot-style payload.
    ///
    /// If the payload does not include `built_in.content_entries`, this is a no-op.
    pub fn sync_content_entries_from_snapshot_payload(
        &mut self,
        collection_uuid: &str,
        thing_uuid: &str,
        payload: &Value,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        let Some(entries_value) = payload
            .get("built_in")
            .and_then(|built_in| built_in.get("content_entries"))
        else {
            return Ok(Vec::new());
        };

        if !entries_value.is_array() {
            anyhow::bail!("built_in.content_entries must be an array when present");
        }

        let desired_entries =
            ContentTypeRegistry::new().extract_content_entries_from_snapshot_data(payload)?;
        let existing_entries = self.get_content_entries(collection_uuid, thing_uuid)?;
        let existing_by_id = existing_entries
            .iter()
            .map(|entry| (entry.id.clone(), entry.clone()))
            .collect::<HashMap<_, _>>();
        let desired_ids = desired_entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<HashSet<_>>();

        let mut events = Vec::new();

        for existing_entry in &existing_entries {
            if !desired_ids.contains(&existing_entry.id) {
                events.extend(self.delete_content_entry(
                    collection_uuid,
                    thing_uuid,
                    &existing_entry.id,
                )?);
            }
        }

        for desired_entry in desired_entries {
            match existing_by_id.get(&desired_entry.id) {
                Some(existing_entry) => {
                    let title = (existing_entry.title != desired_entry.title)
                        .then_some(desired_entry.title.clone());
                    let order = (existing_entry.order != desired_entry.order)
                        .then_some(desired_entry.order);
                    let payload = (existing_entry.payload != desired_entry.payload)
                        .then_some(desired_entry.payload.clone());

                    if title.is_some() || order.is_some() || payload.is_some() {
                        events.extend(self.update_content_entry(
                            collection_uuid,
                            thing_uuid,
                            &desired_entry.id,
                            title,
                            order,
                            payload,
                        )?);
                    }
                }
                None => {
                    events.extend(self.add_content_entry(
                        collection_uuid,
                        thing_uuid,
                        desired_entry,
                    )?);
                }
            }
        }

        Ok(events)
    }

    /// Set plain markdown text on a thing using the default markdown payload shape.
    pub fn set_thing_markdown_text(
        &mut self,
        thing_uuid: &str,
        text: &str,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        self.set_thing_content_from_payload(
            thing_uuid,
            &ThingDatatype::Markdown,
            &json!({ "markdown": text }),
        )
    }

    /// Splice text and report whether the markdown content actually changed.
    pub fn try_splice_thing_text(
        &mut self,
        thing_uuid: &str,
        block_id: &str,
        index: usize,
        delete: usize,
        insert: &str,
    ) -> Result<Option<Vec<ThingsDocumentEvent>>> {
        let before = self.domain_reader().thing_markdown_view(thing_uuid)?;
        let had_content = before.content.is_some();
        let existed = self.thing_markdown_document_exists(thing_uuid);

        let missing_primary_block = block_id == "main"
            && before
                .content
                .as_ref()
                .and_then(|content| content.blocks.as_ref())
                .map(|blocks| !blocks.iter().any(|block| block.id == "main"))
                .unwrap_or(true);

        if missing_primary_block {
            if index == 0 && (delete == 0 || delete == usize::MAX) {
                return Ok(Some(self.replace_thing_markdown_text(thing_uuid, insert)?));
            }

            return Ok(None);
        }

        self.domain_writer()
            .splice_thing_text(thing_uuid, block_id, index, delete, insert)?;

        let after = self.domain_reader().thing_markdown_view(thing_uuid)?;
        if before.content != after.content || !had_content {
            Ok(Some(vec![ThingsDocumentEvent::thing_markdown(
                if existed {
                    ThingsDocumentChangeKind::Updated
                } else {
                    ThingsDocumentChangeKind::Created
                },
                self.find_thing_collection_uuid(thing_uuid).as_deref(),
                thing_uuid,
            )]))
        } else {
            Ok(None)
        }
    }

    /// Replace the entire primary markdown block for a thing.
    pub fn replace_thing_markdown_text(
        &mut self,
        thing_uuid: &str,
        text: &str,
    ) -> Result<Vec<ThingsDocumentEvent>> {
        self.set_thing_markdown_text(thing_uuid, text)
    }

    /// Get plain markdown text from the primary markdown block, if present.
    pub fn get_thing_markdown_text(&self, thing_uuid: &str) -> Result<Option<String>> {
        let md_view = self.domain_reader().thing_markdown_view(thing_uuid)?;
        Ok(md_view.content.and_then(|content| {
            content.blocks.and_then(|blocks| {
                blocks
                    .into_iter()
                    .find(|block| block.id == "main")
                    .and_then(|block| block.text)
            })
        }))
    }

    pub fn extract_tree_data(&self) -> Result<ThingsTreeData> {
        let mut collections = Vec::new();
        let mut things = Vec::new();

        let mut collection_views = Vec::new();
        for (key, state) in self.documents.iter() {
            if key.data_type != CrdtDataType::Collection {
                continue;
            }

            let view = extract_collection_doc_view(&state.automerge_doc, &key.uuid)?;
            let deleted = view
                .meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false);
            if deleted {
                continue;
            }

            collection_views.push((key.uuid.clone(), view));
        }

        collection_views.sort_by(|(left, _), (right, _)| left.cmp(right));

        for (collection_uuid, coll_view) in collection_views {
            let deleted = coll_view
                .meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false);
            if deleted {
                continue;
            }

            collections.push(TreeCollectionData {
                uuid: collection_uuid.clone(),
                title: coll_view.meta.title.clone(),
                trigger_uuid: desired_trigger_uuid(deleted, &coll_view.meta.trigger),
            });

            for thing_meta in coll_view.things {
                let thing_deleted = thing_meta
                    .tombstone
                    .as_ref()
                    .map(|t| t.deleted)
                    .unwrap_or(false);
                if thing_deleted {
                    continue;
                }

                things.push(TreeThingData {
                    uuid: thing_meta.id.clone(),
                    title: thing_meta.title.clone().unwrap_or_default(),
                    status: thing_meta.status.as_storage_str().to_string(),
                    trigger_uuid: desired_trigger_uuid(thing_deleted, &thing_meta.trigger),
                    collection_uuid: collection_uuid.clone(),
                    parent_uuid: thing_meta.parent_id.clone(),
                    entries: thing_meta.built_in.content_entries.clone(),
                });
            }
        }

        Ok(ThingsTreeData {
            collections,
            things,
        })
    }

    // ===== Snapshot Generation =====

    pub fn set_thing_json_content(
        &mut self,
        document_uuid: &str,
        thing_uuid: &str,
        content_type: &str,
        value: &Value,
    ) -> Result<()> {
        let payload_json = serde_json::to_string(value)
            .context("Failed to serialize thing content JSON payload")?;
        self.domain_writer().set_thing_content_document(
            document_uuid,
            thing_uuid,
            content_type,
            Content::Opaque {
                kind: content_type.to_string(),
                payload_json,
            },
        )
    }

    pub fn get_thing_json_content(
        &self,
        document_uuid: &str,
        thing_uuid: &str,
    ) -> Result<Option<Value>> {
        let Some(view) = self
            .domain_reader()
            .thing_content_view(document_uuid, thing_uuid)?
        else {
            return Ok(None);
        };

        Ok(view.content.and_then(|content| content.payload))
    }

    pub fn thing_content_document_uuids(&self, thing_uuid: &str) -> Result<Vec<String>> {
        self.domain_reader()
            .thing_content_document_uuids(thing_uuid)
    }

    /// Extract a full snapshot from all documents
    pub fn extract_snapshot(&self) -> Result<ThingsSnapshot> {
        self.domain_reader().extract_snapshot()
    }

    /// Extract a snapshot with options
    pub fn extract_snapshot_with_options(
        &self,
        options: SnapshotOptions,
    ) -> Result<ThingsSnapshot> {
        self.domain_reader().extract_snapshot_with_options(options)
    }
}

#[cfg(test)]
mod tests_v3 {
    use super::*;

    #[test]
    fn test_document_set_basic() {
        let mut docs = ThingsDocumentSet::new("test-device");
        docs.init_root().unwrap();

        // Add a collection
        docs.get_or_init_collection("coll-1").unwrap();
        let root = docs.root_view().unwrap();
        assert!(root.collection_uuids.contains(&"coll-1".to_string()));

        // Add a thing
        docs.upsert_thing_meta(
            "coll-1",
            "thing-1",
            Some(ThingDatatype::Markdown),
            Some("none".to_string()),
            Some("My Task".to_string()),
            None,
            TriggerUpdate::Noop,
        )
        .unwrap();

        let coll = docs.collection_view("coll-1").unwrap();
        assert_eq!(coll.things.len(), 1);
        assert_eq!(coll.things[0].id, "thing-1");
    }

    #[test]
    fn test_document_set_snapshot() {
        let mut docs = ThingsDocumentSet::new("test-device");
        docs.get_or_init_collection("coll-1").unwrap();
        docs.update_collection_meta_with_timestamps(
            "coll-1",
            Some("My Collection".to_string()),
            None,
            TriggerUpdate::Noop,
            Some("2026-01-01T00:00:00Z".to_string()),
            Some("2026-01-02T00:00:00Z".to_string()),
        )
        .unwrap();
        docs.upsert_thing_meta_with_timestamps(
            "coll-1",
            "thing-1",
            Some(ThingDatatype::Markdown),
            Some("none".to_string()),
            Some("Task 1".to_string()),
            None,
            TriggerUpdate::Noop,
            Some("2026-01-03T00:00:00Z".to_string()),
            Some("2026-01-04T00:00:00Z".to_string()),
        )
        .unwrap();

        let snapshot = docs
            .extract_snapshot_with_options(SnapshotOptions {
                include_content: false,
            })
            .unwrap();
        assert_eq!(snapshot.collections.len(), 1);
        assert_eq!(snapshot.collections[0].title, "My Collection");
        assert_eq!(snapshot.collections[0].created_at, "2026-01-01T00:00:00Z");
        assert_eq!(snapshot.collections[0].updated_at, "2026-01-02T00:00:00Z");
        assert_eq!(snapshot.things.len(), 1);
        assert_eq!(snapshot.things[0].title, "Task 1");
        assert_eq!(snapshot.things[0].created_at, "2026-01-03T00:00:00Z");
        assert_eq!(snapshot.things[0].updated_at, "2026-01-04T00:00:00Z");
        assert_eq!(snapshot.things[0].data.get("attrs"), Some(&Value::Null));
    }

    #[test]
    fn test_document_set_preserves_created_at_across_updates() {
        let mut docs = ThingsDocumentSet::new("test-device");
        docs.get_or_init_collection("coll-1").unwrap();
        docs.upsert_thing_meta_with_timestamps(
            "coll-1",
            "thing-1",
            Some(ThingDatatype::Markdown),
            Some("none".to_string()),
            Some("Task 1".to_string()),
            None,
            TriggerUpdate::Noop,
            Some("2026-01-03T00:00:00Z".to_string()),
            Some("2026-01-04T00:00:00Z".to_string()),
        )
        .unwrap();
        docs.upsert_thing_meta_with_timestamps(
            "coll-1",
            "thing-1",
            Some(ThingDatatype::Markdown),
            Some("none".to_string()),
            Some("Task 1 renamed".to_string()),
            None,
            TriggerUpdate::Noop,
            None,
            Some("2026-01-05T00:00:00Z".to_string()),
        )
        .unwrap();

        let snapshot = docs.extract_snapshot().unwrap();
        assert_eq!(snapshot.things.len(), 1);
        assert_eq!(snapshot.things[0].created_at, "2026-01-03T00:00:00Z");
        assert_eq!(snapshot.things[0].updated_at, "2026-01-05T00:00:00Z");
    }

    #[test]
    fn test_document_events_follow_crdt_mutations() {
        let mut docs = ThingsDocumentSet::new("test-device");

        let collection_events = docs
            .update_collection_meta(
                "coll-1",
                Some("Inbox".to_string()),
                None,
                TriggerUpdate::Noop,
            )
            .unwrap();
        assert_eq!(
            collection_events,
            vec![
                ThingsDocumentEvent::root(ThingsDocumentChangeKind::Updated),
                ThingsDocumentEvent::collection(ThingsDocumentChangeKind::Created, "coll-1"),
            ]
        );

        let thing_events = docs
            .upsert_thing_meta(
                "coll-1",
                "thing-1",
                Some(ThingDatatype::Markdown),
                Some("none".to_string()),
                Some("Task".to_string()),
                None,
                TriggerUpdate::Noop,
            )
            .unwrap();
        assert_eq!(
            thing_events,
            vec![ThingsDocumentEvent::thing(
                ThingsDocumentChangeKind::Created,
                "coll-1",
                "thing-1",
            )]
        );

        let markdown_create = docs.set_thing_markdown_text("thing-1", "hello").unwrap();
        assert_eq!(
            markdown_create,
            vec![ThingsDocumentEvent::thing_markdown(
                ThingsDocumentChangeKind::Created,
                Some("coll-1"),
                "thing-1",
            )]
        );

        let markdown_update = docs
            .try_splice_thing_text("thing-1", "main", 5, 0, " world")
            .unwrap()
            .unwrap();
        assert_eq!(
            markdown_update,
            vec![ThingsDocumentEvent::thing_markdown(
                ThingsDocumentChangeKind::Updated,
                Some("coll-1"),
                "thing-1",
            )]
        );

        let entry = ContentEntry {
            id: "entry-1".to_string(),
            title: Some("Example".to_string()),
            order: 0.0,
            payload: ContentEntryPayload::Custom {
                content_type: "test/custom".to_string(),
                data: json!({ "value": 1 }),
            },
        };

        let add_entry_events = docs.add_content_entry("coll-1", "thing-1", entry).unwrap();
        assert_eq!(
            add_entry_events,
            vec![ThingsDocumentEvent::content_entry(
                ThingsDocumentChangeKind::Created,
                "coll-1",
                "thing-1",
                "entry-1",
            )]
        );

        let update_entry_events = docs
            .update_content_entry(
                "coll-1",
                "thing-1",
                "entry-1",
                Some(Some("Renamed".to_string())),
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            update_entry_events,
            vec![ThingsDocumentEvent::content_entry(
                ThingsDocumentChangeKind::Updated,
                "coll-1",
                "thing-1",
                "entry-1",
            )]
        );

        let delete_entry_events = docs
            .delete_content_entry("coll-1", "thing-1", "entry-1")
            .unwrap();
        assert_eq!(
            delete_entry_events,
            vec![ThingsDocumentEvent::content_entry(
                ThingsDocumentChangeKind::Deleted,
                "coll-1",
                "thing-1",
                "entry-1",
            )]
        );

        let delete_collection_events = docs.delete_collection("coll-1").unwrap();
        assert_eq!(
            delete_collection_events,
            vec![
                ThingsDocumentEvent::collection(ThingsDocumentChangeKind::Deleted, "coll-1"),
                ThingsDocumentEvent::root(ThingsDocumentChangeKind::Updated),
            ]
        );
    }

    #[test]
    fn first_splice_creates_main_markdown_block() {
        let mut docs = ThingsDocumentSet::new("test-device");

        let thing_events = docs
            .upsert_thing_meta(
                "coll-1",
                "thing-empty",
                Some(ThingDatatype::Markdown),
                Some("none".to_string()),
                Some("Empty".to_string()),
                None,
                TriggerUpdate::Noop,
            )
            .unwrap();
        assert_eq!(
            thing_events,
            vec![ThingsDocumentEvent::thing(
                ThingsDocumentChangeKind::Created,
                "coll-1",
                "thing-empty",
            )]
        );

        let markdown_create = docs
            .try_splice_thing_text("thing-empty", "main", 0, 0, "hello")
            .unwrap()
            .unwrap();
        assert_eq!(
            markdown_create,
            vec![ThingsDocumentEvent::thing_markdown(
                ThingsDocumentChangeKind::Created,
                Some("coll-1"),
                "thing-empty",
            )]
        );
        assert_eq!(
            docs.get_thing_markdown_text("thing-empty").unwrap(),
            Some("hello".to_string())
        );
    }

    #[test]
    fn replace_markdown_text_creates_main_block_when_missing() {
        let mut docs = ThingsDocumentSet::new("test-device");

        docs.upsert_thing_meta(
            "coll-1",
            "thing-overwrite",
            Some(ThingDatatype::Markdown),
            Some("none".to_string()),
            Some("Overwrite".to_string()),
            None,
            TriggerUpdate::Noop,
        )
        .unwrap();

        let markdown_create = docs
            .replace_thing_markdown_text("thing-overwrite", "seed")
            .unwrap();
        assert_eq!(
            markdown_create,
            vec![ThingsDocumentEvent::thing_markdown(
                ThingsDocumentChangeKind::Created,
                Some("coll-1"),
                "thing-overwrite",
            )]
        );
        assert_eq!(
            docs.get_thing_markdown_text("thing-overwrite").unwrap(),
            Some("seed".to_string())
        );
    }

    #[test]
    fn test_upsert_thing_requires_existing_collection() {
        let mut docs = ThingsDocumentSet::new("test-device");
        let err = docs
            .upsert_thing_meta(
                "missing-coll",
                "thing-1",
                Some(ThingDatatype::Markdown),
                Some("none".to_string()),
                Some("Task 1".to_string()),
                None,
                TriggerUpdate::Noop,
            )
            .expect_err("thing upsert without collection should fail");

        assert!(
            err.to_string()
                .contains("must exist before adding or reparenting things"),
            "{err:?}"
        );
    }

    #[test]
    fn test_deleted_collection_is_not_relinked_to_root() {
        let mut docs = ThingsDocumentSet::new("test-device");
        docs.get_or_init_collection("coll-1").unwrap();
        docs.update_collection_meta(
            "coll-1",
            Some("My Collection".to_string()),
            None,
            TriggerUpdate::Noop,
        )
        .unwrap();
        docs.upsert_thing_meta(
            "coll-1",
            "thing-1",
            Some(ThingDatatype::Markdown),
            Some("none".to_string()),
            Some("Task 1".to_string()),
            None,
            TriggerUpdate::Noop,
        )
        .unwrap();
        docs.set_thing_markdown_text("thing-1", "hello").unwrap();

        docs.delete_collection("coll-1").unwrap();

        let root = docs.root_view().unwrap();
        assert!(!root.collection_uuids.contains(&"coll-1".to_string()));

        let coll = docs.collection_view("coll-1").unwrap();
        assert!(
            coll.meta
                .tombstone
                .as_ref()
                .map(|t| t.deleted)
                .unwrap_or(false)
        );
        assert!(docs.get(&DocumentKey::collection("coll-1")).is_some());
        assert!(docs.get(&DocumentKey::thing_markdown("thing-1")).is_some());

        let repaired = docs.maintain_root_index_from_live_collections().unwrap();
        assert_eq!(repaired, 0);

        let snapshot = docs
            .extract_snapshot_with_options(SnapshotOptions {
                include_content: false,
            })
            .unwrap();
        assert!(snapshot.collections.is_empty());
        assert!(snapshot.things.is_empty());
    }

    #[test]
    fn test_live_reachability_skips_deleted_entities() {
        let mut docs = ThingsDocumentSet::new("test-device");
        docs.get_or_init_collection("coll-1").unwrap();
        docs.upsert_thing_meta(
            "coll-1",
            "thing-1",
            Some(ThingDatatype::Markdown),
            Some("none".to_string()),
            Some("Task 1".to_string()),
            None,
            TriggerUpdate::Noop,
        )
        .unwrap();
        docs.set_thing_markdown_text("thing-1", "hello").unwrap();

        let live_collections = docs.active_collection_uuids().unwrap();
        let live_things = docs.active_thing_uuids().unwrap();
        assert!(live_collections.contains("coll-1"));
        assert!(live_things.contains("thing-1"));

        docs.delete_thing("coll-1", "thing-1").unwrap();
        let live_things = docs.active_thing_uuids().unwrap();
        assert!(!live_things.contains("thing-1"));

        docs.delete_collection("coll-1").unwrap();
        let live_collections = docs.active_collection_uuids().unwrap();
        let live_things = docs.active_thing_uuids().unwrap();
        assert!(!live_collections.contains("coll-1"));
        assert!(!live_things.contains("thing-1"));
    }

    #[test]
    fn test_snapshot_uses_collection_docs_when_root_index_is_stale() {
        let mut docs = ThingsDocumentSet::new("test-device");
        docs.get_or_init_collection("coll-1").unwrap();
        docs.update_collection_meta(
            "coll-1",
            Some("Inbox".to_string()),
            None,
            TriggerUpdate::Noop,
        )
        .unwrap();
        docs.upsert_thing_meta(
            "coll-1",
            "thing-1",
            Some(ThingDatatype::Markdown),
            Some("none".to_string()),
            Some("Task 1".to_string()),
            None,
            TriggerUpdate::Noop,
        )
        .unwrap();
        docs.set_thing_markdown_text("thing-1", "hello").unwrap();

        docs.remove_collection("coll-1").unwrap();

        let snapshot = docs.extract_snapshot().unwrap();

        assert_eq!(snapshot.collections.len(), 1);
        assert_eq!(snapshot.collections[0].uuid, "coll-1");
        assert_eq!(snapshot.things.len(), 1);
        assert_eq!(snapshot.things[0].uuid, "thing-1");
    }
}

// ============================================================================
// Storage Integration Helpers
// ============================================================================

pub trait CrdtDocumentRepository {
    fn list_crdt_documents(&self) -> Result<Vec<crate::types::CrdtDocumentRow>>;
    fn list_crdt_document_keys(&self) -> Result<Vec<(String, String)>>;
    fn get_crdt_document(
        &self,
        uuid: &str,
        data_type: &str,
    ) -> Result<Option<crate::types::CrdtDocumentRow>>;
    fn save_crdt_document(
        &self,
        uuid: &str,
        data_type: &str,
        automerge_doc: &[u8],
        sync_state: &[u8],
        dirty: bool,
        last_sync_at: Option<&str>,
    ) -> Result<()>;
    fn delete_crdt_document(&self, uuid: &str, data_type: &str) -> Result<()>;
}

impl CrdtDocumentRepository for crate::storage::Storage {
    fn list_crdt_documents(&self) -> Result<Vec<crate::types::CrdtDocumentRow>> {
        self.list_crdt_documents()
    }

    fn list_crdt_document_keys(&self) -> Result<Vec<(String, String)>> {
        self.list_crdt_document_keys()
    }

    fn get_crdt_document(
        &self,
        uuid: &str,
        data_type: &str,
    ) -> Result<Option<crate::types::CrdtDocumentRow>> {
        self.get_crdt_document(uuid, data_type)
    }

    fn save_crdt_document(
        &self,
        uuid: &str,
        data_type: &str,
        automerge_doc: &[u8],
        sync_state: &[u8],
        dirty: bool,
        last_sync_at: Option<&str>,
    ) -> Result<()> {
        self.save_crdt_document(
            uuid,
            data_type,
            automerge_doc,
            sync_state,
            dirty,
            last_sync_at,
        )
    }

    fn delete_crdt_document(&self, uuid: &str, data_type: &str) -> Result<()> {
        self.delete_crdt_document(uuid, data_type)
    }
}

impl CrdtDocumentRepository for crate::TriggerSdk {
    fn list_crdt_documents(&self) -> Result<Vec<crate::types::CrdtDocumentRow>> {
        self.crdt_list_documents()
    }

    fn list_crdt_document_keys(&self) -> Result<Vec<(String, String)>> {
        self.crdt_list_document_keys()
    }

    fn get_crdt_document(
        &self,
        uuid: &str,
        data_type: &str,
    ) -> Result<Option<crate::types::CrdtDocumentRow>> {
        self.crdt_get_document(uuid, data_type)
    }

    fn save_crdt_document(
        &self,
        uuid: &str,
        data_type: &str,
        automerge_doc: &[u8],
        sync_state: &[u8],
        dirty: bool,
        last_sync_at: Option<&str>,
    ) -> Result<()> {
        self.crdt_save_document(
            uuid,
            data_type,
            automerge_doc,
            sync_state,
            dirty,
            last_sync_at,
        )
    }

    fn delete_crdt_document(&self, uuid: &str, data_type: &str) -> Result<()> {
        self.crdt_delete_document(uuid, data_type)
    }
}

pub struct DocumentPersistence<'a, R: CrdtDocumentRepository + ?Sized> {
    repository: &'a R,
}

impl<'a, R: CrdtDocumentRepository + ?Sized> DocumentPersistence<'a, R> {
    pub fn new(repository: &'a R) -> Self {
        Self { repository }
    }

    pub fn load_or_init_document_set(&self, device_id: &str) -> Result<ThingsDocumentSet> {
        let mut doc_set = self.load_document_set(device_id)?;
        if !doc_set.has_root_document() {
            doc_set.init_root()?;
            self.save_document_set(&doc_set)?;
        }
        Ok(doc_set)
    }

    pub fn load_document_set(&self, device_id: &str) -> Result<ThingsDocumentSet> {
        let mut doc_set = ThingsDocumentSet::new(device_id);

        let rows = self
            .repository
            .list_crdt_documents()
            .context("Failed to list CRDT documents")?;

        for row in rows {
            let data_type: CrdtDataType = match row.data_type.as_str() {
                "root" => CrdtDataType::Root,
                "collection" => CrdtDataType::Collection,
                "thing_markdown" => CrdtDataType::ThingMarkdown,
                _ => continue,
            };

            let key = DocumentKey {
                uuid: row.uuid,
                data_type,
            };
            doc_set.set(
                key,
                DocumentState {
                    automerge_doc: row.automerge_doc,
                    sync_state: row.sync_state,
                    dirty: row.dirty,
                    last_sync_at: row.last_sync_at,
                },
            );
        }

        Ok(doc_set)
    }

    pub fn save_document_set(&self, doc_set: &ThingsDocumentSet) -> Result<()> {
        for (key, state) in &doc_set.documents {
            self.repository
                .save_crdt_document(
                    &key.uuid,
                    key.data_type_str(),
                    &state.automerge_doc,
                    &state.sync_state,
                    state.dirty,
                    state.last_sync_at.as_deref(),
                )
                .with_context(|| format!("Failed to save CRDT document {:?}", key))?;
        }
        Ok(())
    }

    pub fn save_dirty_documents(&self, doc_set: &ThingsDocumentSet) -> Result<usize> {
        let dirty = doc_set.dirty_documents();
        let count = dirty.len();

        for (key, state) in dirty {
            self.repository
                .save_crdt_document(
                    &key.uuid,
                    key.data_type_str(),
                    &state.automerge_doc,
                    &state.sync_state,
                    state.dirty,
                    state.last_sync_at.as_deref(),
                )
                .with_context(|| format!("Failed to save dirty CRDT document {:?}", key))?;
        }

        Ok(count)
    }

    pub fn save_dirty_documents_with_compaction(
        &self,
        doc_set: &mut ThingsDocumentSet,
        threshold: usize,
    ) -> Result<(usize, usize)> {
        let dirty_keys = doc_set.store_mut().dirty_document_keys();

        let count = dirty_keys.len();
        let mut compacted = 0;

        for key in &dirty_keys {
            if doc_set.maybe_compact_with_threshold(key, threshold)? {
                compacted += 1;
            }

            if let Some(state) = doc_set.documents.get(key) {
                self.repository
                    .save_crdt_document(
                        &key.uuid,
                        key.data_type_str(),
                        &state.automerge_doc,
                        &state.sync_state,
                        state.dirty,
                        state.last_sync_at.as_deref(),
                    )
                    .with_context(|| format!("Failed to save dirty CRDT document {:?}", key))?;
            }
        }

        Ok((count, compacted))
    }

    pub fn save_document_state(
        &self,
        doc_set: &mut ThingsDocumentSet,
        key: &DocumentKey,
        mark_clean: bool,
        last_sync_at: Option<&str>,
    ) -> Result<()> {
        if let Some(state) = doc_set.documents.get_mut(key) {
            let dirty = if mark_clean { false } else { state.dirty };
            self.repository
                .save_crdt_document(
                    &key.uuid,
                    key.data_type_str(),
                    &state.automerge_doc,
                    &state.sync_state,
                    dirty,
                    last_sync_at.or(state.last_sync_at.as_deref()),
                )
                .with_context(|| format!("Failed to save CRDT document {:?}", key))?;
        }
        doc_set
            .store_mut()
            .set_persisted_state(key, mark_clean, last_sync_at);
        Ok(())
    }

    pub fn delete_collection_documents(
        &self,
        doc_set: &mut ThingsDocumentSet,
        collection_uuid: &str,
    ) -> Result<()> {
        let coll_view = doc_set.collection_view(collection_uuid)?;
        let thing_uuids: Vec<String> = coll_view.things.iter().map(|t| t.id.clone()).collect();

        for thing_uuid in &thing_uuids {
            for document_uuid in doc_set.thing_content_document_uuids(thing_uuid)? {
                let key = DocumentKey::thing_content(&document_uuid);
                doc_set.remove_document(&key);
                self.repository
                    .delete_crdt_document(&document_uuid, "thing_markdown")
                    .ok();
            }
        }

        let key = DocumentKey::collection(collection_uuid);
        doc_set.remove_document(&key);
        self.repository
            .delete_crdt_document(collection_uuid, "collection")?;

        doc_set.remove_collection(collection_uuid)?;

        Ok(())
    }
}

impl ThingsDocumentSet {
    /// Load all documents from storage into a ThingsDocumentSet
    pub fn load_from_storage(storage: &crate::storage::Storage, device_id: &str) -> Result<Self> {
        DocumentPersistence::new(storage).load_document_set(device_id)
    }

    /// Save all documents to storage
    pub fn save_to_storage(&self, storage: &crate::storage::Storage) -> Result<()> {
        DocumentPersistence::new(storage).save_document_set(self)
    }

    /// Save only modified documents to storage
    pub fn save_dirty_to_storage(&self, storage: &crate::storage::Storage) -> Result<usize> {
        DocumentPersistence::new(storage).save_dirty_documents(self)
    }

    /// Save only modified documents to storage, compacting large documents first.
    /// Returns (documents_saved, documents_compacted).
    pub fn save_dirty_to_storage_with_compaction(
        &mut self,
        storage: &crate::storage::Storage,
    ) -> Result<(usize, usize)> {
        self.save_dirty_to_storage_with_compaction_threshold(storage, DEFAULT_COMPACTION_THRESHOLD)
    }

    /// Save only modified documents to storage, compacting documents that exceed the threshold.
    /// Returns (documents_saved, documents_compacted).
    pub fn save_dirty_to_storage_with_compaction_threshold(
        &mut self,
        storage: &crate::storage::Storage,
        threshold: usize,
    ) -> Result<(usize, usize)> {
        DocumentPersistence::new(storage).save_dirty_documents_with_compaction(self, threshold)
    }

    /// Check if any document has pending changes
    pub fn has_pending_changes(&self) -> bool {
        self.store_view().documents.values().any(|s| s.dirty)
    }

    /// Delete a collection and its associated things from storage
    pub fn delete_collection_from_storage(
        &mut self,
        storage: &crate::storage::Storage,
        collection_uuid: &str,
    ) -> Result<()> {
        DocumentPersistence::new(storage).delete_collection_documents(self, collection_uuid)
    }
    /// Make documents field accessible for deletion
    pub fn remove_document(&mut self, key: &DocumentKey) -> Option<DocumentState> {
        self.store_mut().remove_document(key)
    }
}
