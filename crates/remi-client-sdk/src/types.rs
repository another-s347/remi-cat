use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::str::FromStr;

// ============================================================================
// Things Change Log Types
// ============================================================================

/// Operation type for things change log
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThingsOperationType {
    // Collection operations
    CreateCollection,
    UpdateCollection,
    DeleteCollection,
    // Thing operations
    CreateThing,
    UpdateThing,
    DeleteThing,
    MoveThing,
    // Batch operations
    MoveThings,
    DeleteThings,
    // Undo/Redo operations
    UndoCreateCollection,
    UndoUpdateCollection,
    UndoDeleteCollection,
    UndoCreateThing,
    UndoUpdateThing,
    UndoDeleteThing,
    UndoMoveThing,
    UndoMoveThings,
    UndoDeleteThings,
    RedoCreateCollection,
    RedoUpdateCollection,
    RedoDeleteCollection,
    RedoCreateThing,
    RedoUpdateThing,
    RedoDeleteThing,
    RedoMoveThing,
    RedoMoveThings,
    RedoDeleteThings,
}

impl ThingsOperationType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CreateCollection => "create_collection",
            Self::UpdateCollection => "update_collection",
            Self::DeleteCollection => "delete_collection",
            Self::CreateThing => "create_thing",
            Self::UpdateThing => "update_thing",
            Self::DeleteThing => "delete_thing",
            Self::MoveThing => "move_thing",
            Self::MoveThings => "move_things",
            Self::DeleteThings => "delete_things",
            Self::UndoCreateCollection => "undo_create_collection",
            Self::UndoUpdateCollection => "undo_update_collection",
            Self::UndoDeleteCollection => "undo_delete_collection",
            Self::UndoCreateThing => "undo_create_thing",
            Self::UndoUpdateThing => "undo_update_thing",
            Self::UndoDeleteThing => "undo_delete_thing",
            Self::UndoMoveThing => "undo_move_thing",
            Self::UndoMoveThings => "undo_move_things",
            Self::UndoDeleteThings => "undo_delete_things",
            Self::RedoCreateCollection => "redo_create_collection",
            Self::RedoUpdateCollection => "redo_update_collection",
            Self::RedoDeleteCollection => "redo_delete_collection",
            Self::RedoCreateThing => "redo_create_thing",
            Self::RedoUpdateThing => "redo_update_thing",
            Self::RedoDeleteThing => "redo_delete_thing",
            Self::RedoMoveThing => "redo_move_thing",
            Self::RedoMoveThings => "redo_move_things",
            Self::RedoDeleteThings => "redo_delete_things",
        }
    }

    /// Returns true if this is an undo operation type
    pub fn is_undo(&self) -> bool {
        matches!(
            self,
            Self::UndoCreateCollection
                | Self::UndoUpdateCollection
                | Self::UndoDeleteCollection
                | Self::UndoCreateThing
                | Self::UndoUpdateThing
                | Self::UndoDeleteThing
                | Self::UndoMoveThing
                | Self::UndoMoveThings
                | Self::UndoDeleteThings
        )
    }

    /// Returns true if this is a redo operation type
    pub fn is_redo(&self) -> bool {
        matches!(
            self,
            Self::RedoCreateCollection
                | Self::RedoUpdateCollection
                | Self::RedoDeleteCollection
                | Self::RedoCreateThing
                | Self::RedoUpdateThing
                | Self::RedoDeleteThing
                | Self::RedoMoveThing
                | Self::RedoMoveThings
                | Self::RedoDeleteThings
        )
    }

    /// Get the undo variant for this operation type
    pub fn to_undo_variant(&self) -> Option<Self> {
        match self {
            Self::CreateCollection => Some(Self::UndoCreateCollection),
            Self::UpdateCollection => Some(Self::UndoUpdateCollection),
            Self::DeleteCollection => Some(Self::UndoDeleteCollection),
            Self::CreateThing => Some(Self::UndoCreateThing),
            Self::UpdateThing => Some(Self::UndoUpdateThing),
            Self::DeleteThing => Some(Self::UndoDeleteThing),
            Self::MoveThing => Some(Self::UndoMoveThing),
            Self::MoveThings => Some(Self::UndoMoveThings),
            Self::DeleteThings => Some(Self::UndoDeleteThings),
            _ => None, // Already undo/redo variants
        }
    }

    /// Get the redo variant for this operation type (for undo operations)
    pub fn to_redo_variant(&self) -> Option<Self> {
        match self {
            Self::UndoCreateCollection => Some(Self::RedoCreateCollection),
            Self::UndoUpdateCollection => Some(Self::RedoUpdateCollection),
            Self::UndoDeleteCollection => Some(Self::RedoDeleteCollection),
            Self::UndoCreateThing => Some(Self::RedoCreateThing),
            Self::UndoUpdateThing => Some(Self::RedoUpdateThing),
            Self::UndoDeleteThing => Some(Self::RedoDeleteThing),
            Self::UndoMoveThing => Some(Self::RedoMoveThing),
            Self::UndoMoveThings => Some(Self::RedoMoveThings),
            Self::UndoDeleteThings => Some(Self::RedoDeleteThings),
            _ => None,
        }
    }
}

impl fmt::Display for ThingsOperationType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ThingsOperationType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "create_collection" => Ok(Self::CreateCollection),
            "update_collection" => Ok(Self::UpdateCollection),
            "delete_collection" => Ok(Self::DeleteCollection),
            "create_thing" => Ok(Self::CreateThing),
            "update_thing" => Ok(Self::UpdateThing),
            "delete_thing" => Ok(Self::DeleteThing),
            "move_thing" => Ok(Self::MoveThing),
            "move_things" => Ok(Self::MoveThings),
            "delete_things" => Ok(Self::DeleteThings),
            "undo_create_collection" => Ok(Self::UndoCreateCollection),
            "undo_update_collection" => Ok(Self::UndoUpdateCollection),
            "undo_delete_collection" => Ok(Self::UndoDeleteCollection),
            "undo_create_thing" => Ok(Self::UndoCreateThing),
            "undo_update_thing" => Ok(Self::UndoUpdateThing),
            "undo_delete_thing" => Ok(Self::UndoDeleteThing),
            "undo_move_thing" => Ok(Self::UndoMoveThing),
            "undo_move_things" => Ok(Self::UndoMoveThings),
            "undo_delete_things" => Ok(Self::UndoDeleteThings),
            "redo_create_collection" => Ok(Self::RedoCreateCollection),
            "redo_update_collection" => Ok(Self::RedoUpdateCollection),
            "redo_delete_collection" => Ok(Self::RedoDeleteCollection),
            "redo_create_thing" => Ok(Self::RedoCreateThing),
            "redo_update_thing" => Ok(Self::RedoUpdateThing),
            "redo_delete_thing" => Ok(Self::RedoDeleteThing),
            "redo_move_thing" => Ok(Self::RedoMoveThing),
            "redo_move_things" => Ok(Self::RedoMoveThings),
            "redo_delete_things" => Ok(Self::RedoDeleteThings),
            _ => Err(format!("Unknown operation type: {s}")),
        }
    }
}

/// A single entry in the things change log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingsChangeLogEntry {
    /// Auto-increment primary key
    pub id: i64,
    /// Device ID that performed the operation
    pub device_id: String,
    /// Operation type
    pub op_type: ThingsOperationType,
    /// Target entity type: "collection" or "thing"
    pub entity_type: String,
    /// UUID of the primary target entity
    pub entity_uuid: String,
    /// Human-readable summary (e.g., "Deleted 'Shopping List'")
    pub summary: String,
    /// JSON blob with operation-specific details
    pub details_json: String,
    /// For cascade operations: the parent log entry ID that triggered this
    pub parent_log_id: Option<i64>,
    /// For operations that trigger cascades: comma-separated child log IDs
    pub cascade_log_ids_json: Option<String>,
    /// Timestamp when the operation was performed
    pub created_at: DateTime<Utc>,
    /// Whether this operation can be undone
    pub can_undo: bool,
    /// Whether this entry has been synced to server
    pub synced: bool,
}

/// Content snapshot for edit operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingsContentSnapshot {
    /// Auto-increment primary key
    pub id: i64,
    /// Device ID that created the snapshot
    pub device_id: String,
    /// UUID of the thing
    pub thing_uuid: String,
    /// Full JSON content at snapshot time
    pub content_json: String,
    /// Related change log entry ID (for grouping)
    pub change_log_id: Option<i64>,
    /// Timestamp when the snapshot was taken
    pub created_at: DateTime<Utc>,
    /// Whether this snapshot has been synced to server
    pub synced: bool,
}

/// Preview information for an undo operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingsUndoPreview {
    /// The log entry to be undone
    pub log_entry: ThingsChangeLogEntry,
    /// Whether cascade restore is needed (deleted parent was restored)
    pub needs_cascade_restore: bool,
    /// Conflict info if entity still exists or parent is deleted
    pub conflict: Option<ThingsUndoConflict>,
    /// List of cascade operations that would be undone together
    pub cascade_entries: Vec<ThingsChangeLogEntry>,
}

/// Conflict information for undo operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingsUndoConflict {
    /// Type of conflict
    pub conflict_type: ThingsUndoConflictType,
    /// Human-readable description
    pub description: String,
    /// Available resolution options
    pub options: Vec<ThingsUndoResolutionOption>,
}

/// Types of conflicts that can occur during undo
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThingsUndoConflictType {
    /// Trying to restore a deleted entity but parent collection is also deleted
    ParentDeleted,
    /// Trying to undo creation but entity has been modified
    EntityModified,
    /// Trying to restore but an entity with same UUID already exists
    EntityExists,
}

/// Resolution options for undo conflicts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingsUndoResolutionOption {
    /// Unique identifier for this option
    pub id: String,
    /// Human-readable label
    pub label: String,
    /// Detailed description of what this option does
    pub description: String,
}

/// Parameters for executing an undo with conflict resolution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThingsUndoExecution {
    /// Log entry ID to undo
    pub log_id: i64,
    /// Chosen resolution option ID (if conflict exists)
    pub resolution_option: Option<String>,
    /// For move resolution: target collection UUID
    pub target_collection_uuid: Option<String>,
}

/// Generic event payload - event type definitions are decoupled from SDK.
/// The SDK stores and manages abstract events; concrete event types (schemas,
/// validation, special handling) are defined on the application side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventPayload {
    /// Event type identifier (e.g., "Connectivity", "Location", "System")
    #[serde(rename = "type")]
    pub event_type: String,
    /// Timestamp when the event occurred
    #[serde(with = "event_timestamp")]
    pub timestamp: DateTime<Utc>,
    /// Event metadata as JSON object - structure is defined by event type
    #[serde(default)]
    pub metadata: Value,
}

/// Stored event representation (from database)
#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub event_type: String,
    pub timestamp: DateTime<Utc>,
    pub metadata: Value,
}

impl From<StoredEvent> for EventPayload {
    fn from(value: StoredEvent) -> Self {
        Self {
            event_type: value.event_type,
            timestamp: value.timestamp,
            metadata: value.metadata,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRule {
    pub rule: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRegistration {
    pub trigger_uuid: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    pub precondition: Vec<TriggerRule>,
    pub condition: Vec<TriggerRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_uuid: Option<String>,
    #[serde(default = "default_empty_object")]
    pub action_args: Value,
}

#[derive(Debug, Clone)]
pub struct StoredTrigger {
    pub trigger_uuid: String,
    pub name: String,
    pub version: String,
    pub precondition_json: String,
    pub condition_json: String,
    pub next_fire: Option<DateTime<Utc>>,
    pub action_uuid: Option<String>,
    pub action_args_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionDefinition {
    pub action_uuid: String,
    pub name: String,
    pub title: String,
    pub description: String,
    pub version: String,
    pub category: String,
    pub enabled: bool,
    pub metadata_json: Value,
    pub script_source: String,
    pub input_schema_json: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema_json: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionInvocationSourceKind {
    Trigger,
    CollectionManual,
    ThingManual,
    System,
}

impl ActionInvocationSourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Trigger => "trigger",
            Self::CollectionManual => "collection_manual",
            Self::ThingManual => "thing_manual",
            Self::System => "system",
        }
    }
}

impl fmt::Display for ActionInvocationSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ActionInvocationSourceKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "trigger" => Ok(Self::Trigger),
            "collection_manual" => Ok(Self::CollectionManual),
            "thing_manual" => Ok(Self::ThingManual),
            "system" => Ok(Self::System),
            other => Err(format!("Unsupported action invocation source '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionInvocationRecord {
    pub invocation_uuid: String,
    pub action_uuid: String,
    pub source_kind: ActionInvocationSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_entity_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_entity_uuid: Option<String>,
    pub args_json: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_json: Option<Value>,
    pub console_logs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_json: Option<Value>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EntityActionBinding {
    pub action_uuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_override: Option<String>,
    #[serde(default = "default_empty_object")]
    pub args_json: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolvedEntityActionBinding {
    pub action_uuid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_override: Option<String>,
    #[serde(default = "default_empty_object")]
    pub args_json: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_enabled: Option<bool>,
    #[serde(default)]
    pub action_missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerRunType {
    Automatic,
    Manual,
    Replay,
}

impl TriggerRunType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerRunType::Automatic => "automatic",
            TriggerRunType::Manual => "manual",
            TriggerRunType::Replay => "replay",
        }
    }
}

impl fmt::Display for TriggerRunType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TriggerRunType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "automatic" => Ok(Self::Automatic),
            "manual" => Ok(Self::Manual),
            "replay" => Ok(Self::Replay),
            other => Err(format!("Unsupported trigger run type '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerExecutionSummary {
    pub trigger_id: String,
    pub name: String,
    pub fired_at: DateTime<Utc>,
    pub result: bool,
    pub run_type: TriggerRunType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TriggerLogLevel {
    Info,
    Warning,
    Error,
}

impl TriggerLogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerLogLevel::Info => "info",
            TriggerLogLevel::Warning => "warning",
            TriggerLogLevel::Error => "error",
        }
    }
}

impl fmt::Display for TriggerLogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TriggerLogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "info" => Ok(Self::Info),
            "warning" => Ok(Self::Warning),
            "error" => Ok(Self::Error),
            other => Err(format!("Unsupported trigger log level '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerLogEntry {
    pub trigger_id: String,
    pub level: TriggerLogLevel,
    pub message: String,
    pub fire_time: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub run_type: TriggerRunType,
}

// ===== Notification Types =====

/// The originating subsystem of a notification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NotificationSource {
    Trigger,
    Push,
    System,
    Chat,
}

impl NotificationSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            NotificationSource::Trigger => "trigger",
            NotificationSource::Push => "push",
            NotificationSource::System => "system",
            NotificationSource::Chat => "chat",
        }
    }
}

impl fmt::Display for NotificationSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NotificationSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "trigger" => Ok(Self::Trigger),
            "push" => Ok(Self::Push),
            "system" => Ok(Self::System),
            "chat" => Ok(Self::Chat),
            other => Err(format!("Unsupported notification source '{other}'")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationResponseAction {
    Ok,
    NotRightTime,
}

impl NotificationResponseAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            NotificationResponseAction::Ok => "ok",
            NotificationResponseAction::NotRightTime => "not_right_time",
        }
    }
}

impl fmt::Display for NotificationResponseAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NotificationResponseAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "ok" => Ok(Self::Ok),
            "not_right_time" => Ok(Self::NotRightTime),
            other => Err(format!(
                "Unsupported notification response action '{other}'"
            )),
        }
    }
}

/// A single notification entry persisted in SQLite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationEntry {
    pub id: i64,
    pub source: NotificationSource,
    /// Grouping key – for trigger notifications this is the trigger_uuid.
    pub category: String,
    /// Human-readable title for the notification (e.g. trigger name).
    pub title: String,
    /// Main notification body text.
    pub body: String,
    pub is_read: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_action: Option<NotificationResponseAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub responded_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Aggregated group of notifications sharing the same category.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationGroup {
    pub category: String,
    pub title: String,
    pub source: NotificationSource,
    pub latest_at: DateTime<Utc>,
    pub unread_count: i64,
    pub total_count: i64,
    pub items: Vec<NotificationEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerReplaySummary {
    pub trigger_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub runs_considered: u32,
    pub runs_executed: u32,
    pub runs_succeeded: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub trigger_id: String,
    pub name: String,
    pub version: String,
    pub precondition: Vec<TriggerRule>,
    pub condition: Vec<TriggerRule>,
    pub next_fire: Option<DateTime<Utc>>,
    pub last_result: Option<bool>,
    /// Whether this trigger is paused (won't fire even when due).
    #[serde(default)]
    pub is_paused: bool,
    /// The entity type this trigger is currently bound to ("thing" or "collection"), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_type: Option<String>,
    /// The UUID of the entity this trigger is currently bound to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_uuid: Option<String>,
    /// The UUID of the action bound to this trigger, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_uuid: Option<String>,
    /// Trigger-owned action invocation arguments.
    #[serde(default = "default_empty_object")]
    pub action_args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VirtualFsNodeKind {
    Tree,
    Directory,
    File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualFsReadResult {
    pub path: String,
    pub kind: VirtualFsNodeKind,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualFsProfileStep {
    pub name: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualFsProfileResult {
    pub operation: String,
    pub path: String,
    pub total_ms: u64,
    pub output_bytes: usize,
    pub steps: Vec<VirtualFsProfileStep>,
}

fn default_empty_object() -> Value {
    Value::Object(Default::default())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub session_id: String,
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub message_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSessionUpdate {
    pub session_id: String,
    pub title: Option<String>,
    pub last_activity: DateTime<Utc>,
    pub message_count: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentVersion {
    pub version_id: String,
    pub agent_id: String,
    pub name: String,
    pub raw_markdown: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub applied_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentVersionUpdate {
    pub version_id: String,
    pub name: String,
    pub raw_markdown: String,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalDataset {
    pub dataset_id: String,
    pub name: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalDatasetUpdate {
    pub dataset_id: String,
    pub name: String,
    pub description: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalDatasetSession {
    pub dataset_id: String,
    pub session_id: String,
    pub added_at: DateTime<Utc>,
    pub title: Option<String>,
    pub last_activity: Option<DateTime<Utc>>,
    pub message_count: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalDatasetRun {
    pub run_id: String,
    pub dataset_id: String,
    pub agent_id: String,
    pub agent_version_id: Option<String>,
    pub agent_version_name: Option<String>,
    pub variant_id: String,
    pub variant_label: String,
    pub source_session_count: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalDatasetRunItem {
    pub run_id: String,
    pub dataset_id: String,
    pub session_id: String,
    pub title: Option<String>,
    pub last_activity: Option<DateTime<Utc>>,
    pub message_count: Option<i32>,
    pub final_text: String,
    pub reasoning: Option<String>,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub tool_results_json: String,
    pub done: bool,
    pub cancelled: bool,
    pub interrupted: bool,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalDatasetRunEval {
    pub run_id: String,
    pub dataset_id: String,
    pub session_id: String,
    pub analysis_agent_id: String,
    pub score: String,
    pub summary: String,
    pub rationale: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

mod event_timestamp {
    use super::*;
    use serde::de::{Error as DeError, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(value.timestamp())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TsVisitor;

        impl<'de> Visitor<'de> for TsVisitor {
            type Value = DateTime<Utc>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a unix timestamp in seconds or RFC3339 string")
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                timestamp_from_seconds(value)
                    .ok_or_else(|| DeError::custom(format!("Invalid unix timestamp: {value}")))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                let secs = i64::try_from(value).map_err(|_| {
                    DeError::custom(format!("Unix timestamp out of range: {value}"))
                })?;
                self.visit_i64(secs)
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                self.visit_i64(value as i64)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                if let Ok(int_val) = value.parse::<i64>() {
                    return self.visit_i64(int_val);
                }
                DateTime::parse_from_rfc3339(value)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|err| DeError::custom(format!("Invalid timestamp '{value}': {err}")))
            }
        }

        deserializer.deserialize_any(TsVisitor)
    }

    fn timestamp_from_seconds(secs: i64) -> Option<DateTime<Utc>> {
        Utc.timestamp_opt(secs, 0).single()
    }
}

// ============================================================================
// Location Types for Trigger System
// ============================================================================

/// Coordinate system type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CoordinateSystem {
    #[default]
    Wgs84, // GPS standard coordinate system
    Gcj02, // Chinese GCJ-02 coordinate system (Mars coordinates)
}

impl CoordinateSystem {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wgs84 => "wgs84",
            Self::Gcj02 => "gcj02",
        }
    }
}

impl FromStr for CoordinateSystem {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "wgs84" | "wgs-84" => Ok(Self::Wgs84),
            "gcj02" | "gcj-02" => Ok(Self::Gcj02),
            _ => Err(format!("Unknown coordinate system: {}", s)),
        }
    }
}

/// Location object for CEL expressions in triggers
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Location {
    /// Precise coordinate
    #[serde(rename = "coordinate")]
    Coordinate {
        lat: f64,
        lng: f64,
        /// Coordinate system type, defaults to WGS-84
        #[serde(default)]
        coord_system: CoordinateSystem,
        /// Original place name (if obtained from geocoding)
        #[serde(skip_serializing_if = "Option::is_none")]
        source_name: Option<String>,
    },

    /// Fuzzy location name (requires nearby search)
    #[serde(rename = "fuzzy")]
    FuzzyName {
        name: String,
        place_type: String, // Google Places API type
    },

    /// Invalid location (failed to obtain)
    #[serde(rename = "invalid")]
    Invalid { error: String },
}

impl Location {
    /// Create a WGS-84 coordinate
    pub fn coordinate(lat: f64, lng: f64) -> Self {
        Location::Coordinate {
            lat,
            lng,
            coord_system: CoordinateSystem::Wgs84,
            source_name: None,
        }
    }

    /// Create a WGS-84 coordinate with source name
    pub fn coordinate_from_name(lat: f64, lng: f64, name: impl Into<String>) -> Self {
        Location::Coordinate {
            lat,
            lng,
            coord_system: CoordinateSystem::Wgs84,
            source_name: Some(name.into()),
        }
    }

    /// Create a coordinate with specified coordinate system
    pub fn coordinate_with_system(lat: f64, lng: f64, coord_system: CoordinateSystem) -> Self {
        Location::Coordinate {
            lat,
            lng,
            coord_system,
            source_name: None,
        }
    }

    /// Create a fuzzy location
    pub fn fuzzy(name: impl Into<String>, place_type: impl Into<String>) -> Self {
        Location::FuzzyName {
            name: name.into(),
            place_type: place_type.into(),
        }
    }

    /// Create an invalid location with error
    pub fn invalid(error: impl Into<String>) -> Self {
        Location::Invalid {
            error: error.into(),
        }
    }

    pub fn is_valid(&self) -> bool {
        !matches!(self, Location::Invalid { .. })
    }

    pub fn is_fuzzy(&self) -> bool {
        matches!(self, Location::FuzzyName { .. })
    }

    pub fn as_coordinate(&self) -> Option<(f64, f64)> {
        match self {
            Location::Coordinate { lat, lng, .. } => Some((*lat, *lng)),
            _ => None,
        }
    }

    pub fn coord_system(&self) -> Option<CoordinateSystem> {
        match self {
            Location::Coordinate { coord_system, .. } => Some(*coord_system),
            _ => None,
        }
    }

    pub fn source_name(&self) -> Option<&str> {
        match self {
            Location::Coordinate { source_name, .. } => source_name.as_deref(),
            Location::FuzzyName { name, .. } => Some(name.as_str()),
            _ => None,
        }
    }
}

/// Cached location entry from geocoding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocationCacheEntry {
    pub name: String,
    pub is_fuzzy: bool,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub coord_system: CoordinateSystem,
    pub place_id: Option<String>,
    pub place_type: Option<String>,
    pub formatted_address: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl LocationCacheEntry {
    /// Convert to Location enum
    pub fn to_location(&self) -> Location {
        if self.is_fuzzy {
            Location::FuzzyName {
                name: self.name.clone(),
                place_type: self.place_type.clone().unwrap_or_default(),
            }
        } else if let (Some(lat), Some(lng)) = (self.latitude, self.longitude) {
            Location::Coordinate {
                lat,
                lng,
                coord_system: self.coord_system,
                source_name: Some(self.name.clone()),
            }
        } else {
            Location::Invalid {
                error: format!("Cached location '{}' has no coordinates", self.name),
            }
        }
    }
}

// ============================================================================
// V3 CRDT Document Types
// ============================================================================

/// Row from crdt_documents table
#[derive(Debug, Clone)]
pub struct CrdtDocumentRow {
    pub uuid: String,
    pub data_type: String,
    pub automerge_doc: Vec<u8>,
    pub sync_state: Vec<u8>,
    pub dirty: bool,
    pub last_sync_at: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl CrdtDocumentRow {
    /// Parse data_type string to CrdtDataType enum
    pub fn crdt_data_type(&self) -> Option<remi_things_crdt::CrdtDataType> {
        match self.data_type.as_str() {
            "root" => Some(remi_things_crdt::CrdtDataType::Root),
            "collection" => Some(remi_things_crdt::CrdtDataType::Collection),
            "thing_markdown" => Some(remi_things_crdt::CrdtDataType::ThingMarkdown),
            _ => None,
        }
    }
}
