use serde::{Deserialize, Serialize};

use crate::datatype::{ContentEntry, DateField, LocationField, ThingBuiltInFields};
use crate::ThingDatatype;

// ============================================================================
// Common View Types (used by both legacy and v3)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditClock {
    pub actor: String,
    pub seq: u64,
}

impl EditClock {
    pub fn new(actor: impl Into<String>, seq: u64) -> Self {
        Self {
            actor: actor.into(),
            seq,
        }
    }

    pub fn zero() -> Self {
        Self {
            actor: String::new(),
            seq: 0,
        }
    }
}

impl Ord for EditClock {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.seq.cmp(&other.seq) {
            std::cmp::Ordering::Equal => self.actor.cmp(&other.actor),
            ord => ord,
        }
    }
}

impl PartialOrd for EditClock {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Default for EditClock {
    fn default() -> Self {
        Self::zero()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tombstone {
    pub deleted: bool,
    pub clock: EditClock,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TriggerBinding {
    pub state: String, // "none" | "some"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    pub clock: EditClock,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ThingStatus {
    None,
    InProgress { timestamp_ms: i64 },
    Stalled { timestamp_ms: i64 },
    Done { timestamp_ms: i64 },
}

impl ThingStatus {
    /// Returns the status as a string for storage in the CRDT
    pub fn as_storage_str(&self) -> &str {
        match self {
            ThingStatus::None => "none",
            ThingStatus::InProgress { .. } => "in-progress",
            ThingStatus::Stalled { .. } => "stalled",
            ThingStatus::Done { .. } => "done",
        }
    }

    /// Returns the optional timestamp in milliseconds
    pub fn timestamp_ms(&self) -> Option<i64> {
        match self {
            ThingStatus::None => None,
            ThingStatus::InProgress { timestamp_ms } => Some(*timestamp_ms),
            ThingStatus::Stalled { timestamp_ms } => Some(*timestamp_ms),
            ThingStatus::Done { timestamp_ms } => Some(*timestamp_ms),
        }
    }

    /// Parse from storage string and optional timestamp
    pub fn from_storage(status_str: &str, timestamp_ms: Option<i64>) -> Self {
        let now_ms = || chrono::Utc::now().timestamp_millis();

        match status_str {
            "in-progress" => ThingStatus::InProgress {
                timestamp_ms: timestamp_ms.unwrap_or_else(now_ms),
            },
            "stalled" => ThingStatus::Stalled {
                timestamp_ms: timestamp_ms.unwrap_or_else(now_ms),
            },
            "done" => ThingStatus::Done {
                timestamp_ms: timestamp_ms.unwrap_or_else(now_ms),
            },
            _ => ThingStatus::None,
        }
    }
}

impl Default for ThingStatus {
    fn default() -> Self {
        ThingStatus::None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockView {
    pub id: String,
    pub r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContentView {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Vec<BlockView>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

// ============================================================================
// Legacy View Types (v2 single-document architecture)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectionView {
    pub id: String,
    pub title: String,
    pub status: String,
    pub edit_clock: EditClock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<Tombstone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThingView {
    pub id: String,
    pub collection_id: String,
    pub datatype: ThingDatatype,
    pub status: ThingStatus,
    pub edit_clock: EditClock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<Tombstone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Value>,
}

/// Legacy view for v2 single-document architecture
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct View {
    pub schema_version: u32,
    pub epoch: u64,
    pub collections: Vec<CollectionView>,
    pub things: Vec<ThingView>,
}

// ============================================================================
// New View Types (v3 multi-document architecture)
// ============================================================================

/// View for Root document (CrdtDataType::Root)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RootView {
    pub schema_version: u32,
    pub epoch: u64,
    /// List of all collection UUIDs owned by this user
    pub collection_uuids: Vec<String>,
}

/// View for Collection document (CrdtDataType::Collection)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectionDocView {
    pub schema_version: u32,
    /// Collection metadata
    pub meta: CollectionMetaView,
    /// All things in this collection (metadata only, no markdown content)
    pub things: Vec<ThingMetaView>,
}

/// Collection metadata within a Collection document
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectionMetaView {
    pub id: String,
    pub title: String,
    pub status: String,
    pub edit_clock: EditClock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<Tombstone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Value>,
}

/// Thing metadata within a Collection document (no markdown content)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThingMetaView {
    pub id: String,
    pub datatype: ThingDatatype,
    pub status: ThingStatus,
    pub edit_clock: EditClock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tombstone: Option<Tombstone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<TriggerBinding>,
    /// Built-in fields (location, date, markdown_doc_uuid, etc.)
    #[serde(default)]
    pub built_in: ThingBuiltInFieldsView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attrs: Option<serde_json::Value>,
}

/// View of ThingBuiltInFields extracted from CRDT (V3 multi-value)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ThingBuiltInFieldsView {
    /// Content entries (location, markdown, date, etc.) - sorted by order
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_entries: Vec<ContentEntry>,

    /// Extra key-value fields
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

impl ThingBuiltInFieldsView {
    /// Get the first location (for backward compatibility)
    pub fn first_location(&self) -> Option<&LocationField> {
        self.content_entries.iter().find_map(|e| {
            if let crate::datatype::ContentEntryPayload::Location(loc) = &e.payload {
                Some(loc)
            } else {
                None
            }
        })
    }

    /// Get the first date (for backward compatibility)
    pub fn first_date(&self) -> Option<&DateField> {
        self.content_entries.iter().find_map(|e| {
            if let crate::datatype::ContentEntryPayload::Date(date) = &e.payload {
                Some(date)
            } else {
                None
            }
        })
    }

    /// Get the first markdown doc UUID (for backward compatibility)
    pub fn first_markdown_doc_uuid(&self) -> Option<&str> {
        self.content_entries.iter().find_map(|e| {
            if let crate::datatype::ContentEntryPayload::Markdown { doc_uuid } = &e.payload {
                Some(doc_uuid.as_str())
            } else {
                None
            }
        })
    }
}

impl From<ThingBuiltInFields> for ThingBuiltInFieldsView {
    fn from(fields: ThingBuiltInFields) -> Self {
        ThingBuiltInFieldsView {
            content_entries: fields.content_entries,
            extra: fields.extra,
        }
    }
}

impl From<ThingBuiltInFieldsView> for ThingBuiltInFields {
    fn from(view: ThingBuiltInFieldsView) -> Self {
        ThingBuiltInFields {
            content_entries: view.content_entries,
            extra: view.extra,
        }
    }
}

/// View for ThingMarkdown document (CrdtDataType::ThingMarkdown)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThingMarkdownView {
    pub schema_version: u32,
    /// The thing UUID this markdown belongs to
    pub thing_uuid: String,
    /// Markdown content (blocks)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentView>,
}

/// Generic view for a thing-owned content document.
///
/// Today the only concrete content document family is `thing_markdown`, but the
/// long-term model treats that as the first registered content-doc specialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ThingContentView {
    pub schema_version: u32,
    /// Document UUID for this content document.
    pub document_uuid: String,
    /// The thing UUID this content belongs to.
    pub thing_uuid: String,
    /// Registered content type carried by this document, for example `markdown`.
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ContentView>,
}

// ============================================================================
// Conversion helpers
// ============================================================================

impl CollectionMetaView {
    /// Convert to legacy CollectionView (for API compatibility)
    pub fn to_legacy_view(&self) -> CollectionView {
        CollectionView {
            id: self.id.clone(),
            title: self.title.clone(),
            status: self.status.clone(),
            edit_clock: self.edit_clock.clone(),
            tombstone: self.tombstone.clone(),
            trigger: self.trigger.clone(),
            attrs: self.attrs.clone(),
        }
    }
}

impl ThingMetaView {
    /// Convert to legacy ThingView (for API compatibility)
    /// Note: content will be None; caller must populate from ThingMarkdown doc if needed
    pub fn to_legacy_view(&self, collection_id: &str) -> ThingView {
        ThingView {
            id: self.id.clone(),
            collection_id: collection_id.to_string(),
            datatype: self.datatype.clone(),
            status: self.status.clone(),
            edit_clock: self.edit_clock.clone(),
            tombstone: self.tombstone.clone(),
            title: self.title.clone(),
            parent_id: self.parent_id.clone(),
            trigger: self.trigger.clone(),
            content: None, // Must be populated separately from ThingMarkdown doc
            attrs: self.attrs.clone(),
        }
    }
}
