use serde::{Deserialize, Serialize};

use crate::things_crdt::{ThingCollectionEntry, ThingEntry};

/// SDK -> UI event stream for Things updates.
///
/// This is designed to surface lower-level document changes from the CRDT/domain
/// layer while still allowing an explicit full snapshot refresh after sync.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThingsDocumentKind {
    Root,
    Collection,
    Thing,
    ThingMarkdown,
    ContentEntry,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThingsDocumentChangeKind {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThingsDocumentEvent {
    pub document_kind: ThingsDocumentKind,
    pub change_kind: ThingsDocumentChangeKind,
    pub document_uuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thing_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
}

impl ThingsDocumentEvent {
    pub fn root(change_kind: ThingsDocumentChangeKind) -> Self {
        Self {
            document_kind: ThingsDocumentKind::Root,
            change_kind,
            document_uuid: remi_things_crdt::ROOT_DOC_UUID.to_string(),
            collection_uuid: None,
            thing_uuid: None,
            entry_id: None,
        }
    }

    pub fn collection(change_kind: ThingsDocumentChangeKind, collection_uuid: &str) -> Self {
        Self {
            document_kind: ThingsDocumentKind::Collection,
            change_kind,
            document_uuid: collection_uuid.to_string(),
            collection_uuid: Some(collection_uuid.to_string()),
            thing_uuid: None,
            entry_id: None,
        }
    }

    pub fn thing(
        change_kind: ThingsDocumentChangeKind,
        collection_uuid: &str,
        thing_uuid: &str,
    ) -> Self {
        Self {
            document_kind: ThingsDocumentKind::Thing,
            change_kind,
            document_uuid: thing_uuid.to_string(),
            collection_uuid: Some(collection_uuid.to_string()),
            thing_uuid: Some(thing_uuid.to_string()),
            entry_id: None,
        }
    }

    pub fn thing_markdown(
        change_kind: ThingsDocumentChangeKind,
        collection_uuid: Option<&str>,
        thing_uuid: &str,
    ) -> Self {
        Self {
            document_kind: ThingsDocumentKind::ThingMarkdown,
            change_kind,
            document_uuid: thing_uuid.to_string(),
            collection_uuid: collection_uuid.map(|value| value.to_string()),
            thing_uuid: Some(thing_uuid.to_string()),
            entry_id: None,
        }
    }

    pub fn content_entry(
        change_kind: ThingsDocumentChangeKind,
        collection_uuid: &str,
        thing_uuid: &str,
        entry_id: &str,
    ) -> Self {
        Self {
            document_kind: ThingsDocumentKind::ContentEntry,
            change_kind,
            document_uuid: entry_id.to_string(),
            collection_uuid: Some(collection_uuid.to_string()),
            thing_uuid: Some(thing_uuid.to_string()),
            entry_id: Some(entry_id.to_string()),
        }
    }

    pub fn into_event(self, device_id: &str) -> ThingsEvent {
        ThingsEvent::DocumentChanged {
            device_id: device_id.to_string(),
            document: self,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThingsEvent {
    /// Replace the entire Things snapshot in one shot.
    ///
    /// This is used for large state replacement flows (e.g., after sync/bootstrap/recovery)
    /// where emitting per-entity diffs is undesirable.
    SnapshotReplaced {
        device_id: String,
        collections: Vec<ThingCollectionEntry>,
        things: Vec<ThingEntry>,
        dirty: bool,
        last_sync_at: Option<String>,
    },
    DocumentChanged {
        device_id: String,
        #[serde(flatten)]
        document: ThingsDocumentEvent,
    },
    /// All local data has been wiped (logout). UI should clear all state.
    DataWiped,
}

impl ThingsEvent {
    pub fn device_id(&self) -> &str {
        match self {
            ThingsEvent::SnapshotReplaced { device_id, .. }
            | ThingsEvent::DocumentChanged { device_id, .. } => device_id,
            ThingsEvent::DataWiped => "",
        }
    }
}
