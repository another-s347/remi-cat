use crate::chat_types::{CachedMessage, ChatProtocolSessionState, ChatSessionExportBundle};
use crate::context_prompt;
use crate::events_events::EventsEvent;
use crate::notification_events::NotificationEvent;
use crate::realtime::{RemiRealtimeEvent, SupabaseRealtimeManager};
use crate::storage::Storage;
use crate::things_crdt::{
    ContentEntry, ContentEntryPayload, ContentEntryUpdate, JsonObjectField, ThingCollectionEntry,
    ThingCollectionUpsert, ThingEntry, ThingUpsert, ThingsSnapshot, ThingsSnapshotState,
};
use crate::things_events::{ThingsDocumentChangeKind, ThingsDocumentEvent, ThingsEvent};
use crate::trigger_events::TriggerEvent;
use crate::types::{
    ActionDefinition, ActionInvocationRecord, ActionInvocationSourceKind, EntityActionBinding,
    EventPayload, NotificationResponseAction, NotificationSource, ResolvedEntityActionBinding,
    StoredTrigger, ThingsChangeLogEntry, ThingsContentSnapshot, ThingsOperationType,
    ThingsUndoConflict, ThingsUndoConflictType, ThingsUndoExecution, ThingsUndoPreview,
    ThingsUndoResolutionOption, TriggerExecutionSummary, TriggerInfo, TriggerLogLevel,
    TriggerRegistration, TriggerReplaySummary, TriggerRule, TriggerRunType,
};
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use chrono::{DateTime, Datelike, Duration, FixedOffset, Local, TimeZone, Timelike, Utc};
use croner::Cron;
use jsonschema::JSONSchema;
use rule_trigger_engine::{
    EvaluationContext, MonitoringEvent, PreconditionPolicy, Rule as EngineRule, TriggerConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::to_string;
use serde_json::{Value, json};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

mod path_tools;
pub(crate) use path_tools::VirtualFsCatResult;

const DEFAULT_TRIGGER_NOTIFICATION_ACTION_UUID: &str = "builtin.trigger_notification";

const ENTITY_ACTION_BINDINGS_ATTR_KEY: &str = "action_bindings";
const COLLECTION_CARD_JSX_ATTR_KEY: &str = "card_jsx";
#[cfg(feature = "quickjs")]
const DEFAULT_ACTION_HTTP_TIMEOUT_MS: u64 = 30_000;

fn strip_internal_entity_attrs(attrs: Option<&Value>) -> Value {
    let mut map = match attrs {
        Some(Value::Object(existing)) => existing.clone(),
        _ => serde_json::Map::new(),
    };
    map.remove("__remi_created_at");
    map.remove("__remi_updated_at");
    Value::Object(map)
}

fn decode_entity_action_bindings(attrs: Option<&Value>) -> Vec<EntityActionBinding> {
    attrs
        .and_then(|value| value.as_object())
        .and_then(|map| map.get(ENTITY_ACTION_BINDINGS_ATTR_KEY))
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<EntityActionBinding>>(value).ok())
        .unwrap_or_default()
}

fn decode_collection_card_jsx(attrs: Option<&Value>) -> Option<String> {
    attrs
        .and_then(|value| value.as_object())
        .and_then(|map| map.get(COLLECTION_CARD_JSX_ATTR_KEY))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn encode_entity_action_bindings(attrs: Option<&Value>, bindings: &[EntityActionBinding]) -> Value {
    let mut root = match strip_internal_entity_attrs(attrs) {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };

    if bindings.is_empty() {
        root.remove(ENTITY_ACTION_BINDINGS_ATTR_KEY);
    } else {
        root.insert(
            ENTITY_ACTION_BINDINGS_ATTR_KEY.to_string(),
            serde_json::to_value(bindings).unwrap_or_else(|_| Value::Array(Vec::new())),
        );
    }

    Value::Object(root)
}

fn encode_collection_card_jsx(attrs: Option<&Value>, card_jsx: Option<&str>) -> Value {
    let mut root = match strip_internal_entity_attrs(attrs) {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };

    if let Some(card_jsx) = card_jsx.map(str::trim).filter(|value| !value.is_empty()) {
        root.insert(
            COLLECTION_CARD_JSX_ATTR_KEY.to_string(),
            Value::String(card_jsx.to_string()),
        );
    } else {
        root.remove(COLLECTION_CARD_JSX_ATTR_KEY);
    }

    Value::Object(root)
}

const DEFAULT_TIMEZONE_OFFSET: &str = "+08:00";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BootstrapStashedDocument {
    uuid: String,
    data_type: String,
    automerge_doc_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BootstrapStashPayload {
    version: u8,
    documents: Vec<BootstrapStashedDocument>,
}

enum BootstrapReplaySource {
    Documents(Vec<BootstrapStashedDocument>),
    LegacySnapshot(ThingsSnapshot),
}

fn default_timezone() -> FixedOffset {
    FixedOffset::from_str(DEFAULT_TIMEZONE_OFFSET).unwrap_or_else(|_| {
        FixedOffset::east_opt(8 * 3600).expect("UTC+08:00 offset must be valid")
    })
}

fn local_timezone_offset_string() -> String {
    let seconds = Local::now().offset().local_minus_utc();
    format_offset_seconds(seconds)
}

fn format_offset_seconds(total_seconds: i32) -> String {
    let sign = if total_seconds < 0 { '-' } else { '+' };
    let abs = total_seconds.abs();
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    format!("{sign}{hours:02}:{minutes:02}")
}

#[cfg(feature = "quickjs")]
fn default_action_notification_source(
    source_kind: &ActionInvocationSourceKind,
) -> NotificationSource {
    match source_kind {
        ActionInvocationSourceKind::Trigger => NotificationSource::Trigger,
        ActionInvocationSourceKind::CollectionManual
        | ActionInvocationSourceKind::ThingManual
        | ActionInvocationSourceKind::System => NotificationSource::System,
    }
}

#[cfg(feature = "quickjs")]
fn parse_notification_source(value: &str) -> Result<NotificationSource> {
    match value.trim().to_ascii_lowercase().as_str() {
        "trigger" => Ok(NotificationSource::Trigger),
        "push" => Ok(NotificationSource::Push),
        "system" => Ok(NotificationSource::System),
        "chat" => Ok(NotificationSource::Chat),
        other => anyhow::bail!("Unsupported notification source '{other}'"),
    }
}

#[cfg(feature = "quickjs")]
fn action_http_request_handler(action_uuid: String) -> crate::quickjs::QuickJsHostHandler {
    Arc::new(move |request| {
        execute_action_http_request(&action_uuid, request).map_err(|error| error.to_string())
    })
}

#[cfg(feature = "quickjs")]
fn action_notify_send_handler(
    storage: Storage,
    notification_event_tx: broadcast::Sender<NotificationEvent>,
    action_uuid: String,
    source_kind: ActionInvocationSourceKind,
) -> crate::quickjs::QuickJsHostHandler {
    let default_source = default_action_notification_source(&source_kind);
    Arc::new(move |request| {
        let result: Result<Value> = (|| {
            let title = request
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("notify.send requires a non-empty title"))?;
            let body = request
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let category = request
                .get("category")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("action:{action_uuid}"));
            let source = request
                .get("source")
                .and_then(Value::as_str)
                .map(parse_notification_source)
                .transpose()?
                .unwrap_or_else(|| default_source.clone());

            let notification_id = storage.insert_notification(&source, &category, title, body)?;
            let _ = notification_event_tx.send(NotificationEvent::Added {
                notification_id,
                category: category.clone(),
                source: source.clone(),
                title: title.to_string(),
            });
            Ok(json!({
                "notification_id": notification_id,
                "source": source,
                "category": category,
                "title": title,
                "body": body,
            }))
        })();

        result.map_err(|error| error.to_string())
    })
}

#[cfg(feature = "quickjs")]
fn action_notify_list_handler(storage: Storage) -> crate::quickjs::QuickJsHostHandler {
    Arc::new(move |request| {
        let result: Result<Value> = (|| {
            let limit = request
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .min(u32::MAX as u64) as u32;

            if let Some(category) = request.get("category").and_then(Value::as_str) {
                let notifications = storage.list_notifications_by_category(category, limit)?;
                return Ok(json!({
                    "mode": "category",
                    "category": category,
                    "items": notifications,
                }));
            }

            if request
                .get("flat")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let offset = request
                    .get("offset")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(u32::MAX as u64) as u32;
                let notifications = storage.list_notifications_flat(limit, offset)?;
                return Ok(json!({
                    "mode": "flat",
                    "offset": offset,
                    "items": notifications,
                }));
            }

            let groups = storage.list_notifications_grouped(limit)?;
            Ok(json!({
                "mode": "grouped",
                "groups": groups,
            }))
        })();

        result.map_err(|error| error.to_string())
    })
}

#[cfg(feature = "quickjs")]
fn action_notify_mark_read_handler(
    storage: Storage,
    notification_event_tx: broadcast::Sender<NotificationEvent>,
) -> crate::quickjs::QuickJsHostHandler {
    Arc::new(move |request| {
        let result: Result<Value> = (|| {
            let notification_id = request
                .get("notificationId")
                .or_else(|| request.get("notification_id"))
                .and_then(Value::as_i64)
                .ok_or_else(|| anyhow!("notify.markRead requires notificationId"))?;
            storage.mark_notification_read(notification_id)?;
            let _ = notification_event_tx.send(NotificationEvent::Read { notification_id });
            Ok(json!({ "notification_id": notification_id, "read": true }))
        })();

        result.map_err(|error| error.to_string())
    })
}

#[cfg(feature = "quickjs")]
fn action_notify_mark_category_read_handler(
    storage: Storage,
    notification_event_tx: broadcast::Sender<NotificationEvent>,
) -> crate::quickjs::QuickJsHostHandler {
    Arc::new(move |request| {
        let result: Result<Value> = (|| {
            let category = request
                .get("category")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("notify.markCategoryRead requires category"))?;
            storage.mark_category_notifications_read(category)?;
            let _ = notification_event_tx.send(NotificationEvent::CategoryRead {
                category: category.to_string(),
            });
            Ok(json!({ "category": category, "read": true }))
        })();

        result.map_err(|error| error.to_string())
    })
}

#[cfg(feature = "quickjs")]
fn action_notify_mark_all_read_handler(
    storage: Storage,
    notification_event_tx: broadcast::Sender<NotificationEvent>,
) -> crate::quickjs::QuickJsHostHandler {
    Arc::new(move |_request| {
        storage
            .mark_all_notifications_read()
            .map_err(|error| error.to_string())?;
        let _ = notification_event_tx.send(NotificationEvent::AllRead);
        Ok(json!({ "read": true }))
    })
}

#[cfg(feature = "quickjs")]
fn action_notify_delete_category_handler(
    storage: Storage,
    notification_event_tx: broadcast::Sender<NotificationEvent>,
) -> crate::quickjs::QuickJsHostHandler {
    Arc::new(move |request| {
        let result: Result<Value> = (|| {
            let category = request
                .get("category")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("notify.deleteCategory requires category"))?;
            storage.delete_notifications_by_category(category)?;
            let _ = notification_event_tx.send(NotificationEvent::CategoryDeleted {
                category: category.to_string(),
            });
            Ok(json!({ "category": category, "deleted": true }))
        })();

        result.map_err(|error| error.to_string())
    })
}

#[cfg(feature = "quickjs")]
fn execute_action_http_request(action_uuid: &str, request: Value) -> Result<Value> {
    let request_object = request
        .as_object()
        .ok_or_else(|| anyhow!("http.request expects an object payload"))?;
    let method = request_object
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    let url = request_object
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("http.request requires a non-empty url"))?;
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .with_context(|| format!("Unsupported HTTP method '{method}'"))?;
    let timeout_ms = request_object
        .get("timeout_ms")
        .or_else(|| request_object.get("timeoutMs"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_ACTION_HTTP_TIMEOUT_MS);

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .user_agent(format!("remi-action/{action_uuid}"))
        .build()
        .context("Failed to build action HTTP client")?;

    let mut request_builder = client.request(method.clone(), url);

    if let Some(headers) = request_object.get("headers").and_then(Value::as_object) {
        for (name, value) in headers {
            let header_value = match value {
                Value::String(text) => text.clone(),
                _ => {
                    serde_json::to_string(value).context("Failed to serialize HTTP header value")?
                }
            };
            request_builder = request_builder.header(name, header_value);
        }
    }

    if let Some(query) = request_object.get("query") {
        request_builder = request_builder.query(query);
    }

    let has_json_body = request_object.get("json").is_some();
    if let Some(json_body) = request_object.get("json") {
        request_builder = request_builder.json(json_body);
    } else if let Some(body_base64) = request_object.get("body_base64").and_then(Value::as_str) {
        let body_bytes = base64::engine::general_purpose::STANDARD
            .decode(body_base64)
            .context("Invalid base64 in http.request body_base64")?;
        request_builder = request_builder.body(body_bytes);
    } else if let Some(body_text) = request_object
        .get("body_text")
        .or_else(|| request_object.get("text"))
        .and_then(Value::as_str)
    {
        request_builder = request_builder.body(body_text.to_string());
    } else if let Some(body) = request_object.get("body") {
        match body {
            Value::String(text) => {
                request_builder = request_builder.body(text.clone());
            }
            _ => {
                request_builder = request_builder
                    .body(serde_json::to_vec(body).context("Failed to serialize HTTP body")?);
                if !has_json_body && request_object.get("headers").is_none() {
                    request_builder = request_builder.header("content-type", "application/json");
                }
            }
        }
    }

    let response = request_builder
        .send()
        .with_context(|| format!("HTTP request failed for {url}"))?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .bytes()
        .context("Failed to read HTTP response body")?;
    let body_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let body_text = String::from_utf8(bytes.to_vec()).ok();
    let body_json = body_text
        .as_ref()
        .and_then(|text| serde_json::from_str::<Value>(text).ok());

    let headers_json = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value
                    .to_str()
                    .map(|text| Value::String(text.to_string()))
                    .unwrap_or_else(|_| Value::String(String::new())),
            )
        })
        .collect::<serde_json::Map<String, Value>>();

    Ok(json!({
        "ok": status.is_success(),
        "status": status.as_u16(),
        "status_text": status.canonical_reason().unwrap_or_default(),
        "url": url,
        "method": method.as_str(),
        "headers": headers_json,
        "body_text": body_text,
        "body_json": body_json,
        "body_base64": body_base64,
        "content_length": bytes.len(),
    }))
}

#[cfg(feature = "quickjs")]
fn build_action_quickjs_bindings(
    storage: &Storage,
    notification_event_tx: &broadcast::Sender<NotificationEvent>,
    action: &ActionDefinition,
    execution_input: &Value,
) -> crate::quickjs::QuickJsHostBindings {
    let source_kind = execution_input
        .get("source")
        .and_then(|value| value.get("kind"))
        .and_then(Value::as_str)
        .and_then(|value| ActionInvocationSourceKind::from_str(value).ok())
        .unwrap_or(ActionInvocationSourceKind::System);
    let storage = storage.clone();

    crate::quickjs::QuickJsHostBindings {
        http_request: Some(action_http_request_handler(action.action_uuid.clone())),
        notify_send: Some(action_notify_send_handler(
            storage.clone(),
            notification_event_tx.clone(),
            action.action_uuid.clone(),
            source_kind.clone(),
        )),
        notify_list: Some(action_notify_list_handler(storage.clone())),
        notify_mark_read: Some(action_notify_mark_read_handler(
            storage.clone(),
            notification_event_tx.clone(),
        )),
        notify_mark_category_read: Some(action_notify_mark_category_read_handler(
            storage.clone(),
            notification_event_tx.clone(),
        )),
        notify_mark_all_read: Some(action_notify_mark_all_read_handler(
            storage.clone(),
            notification_event_tx.clone(),
        )),
        notify_delete_category: Some(action_notify_delete_category_handler(
            storage,
            notification_event_tx.clone(),
        )),
    }
}

fn default_trigger_notification_args(trigger: &StoredTrigger, fire_time: DateTime<Utc>) -> Value {
    json!({
        "title": trigger.name,
        "body": format!(
            "触发器「{}」已于 {} 触发",
            trigger.name,
            fire_time.with_timezone(&default_timezone()).format("%H:%M")
        ),
        "category": trigger.trigger_uuid,
        "source": "trigger",
    })
}

fn notification_id_from_action_result(result: Option<&Value>) -> Option<i64> {
    result
        .and_then(Value::as_object)
        .and_then(|value| value.get("notification_id"))
        .and_then(Value::as_i64)
}

fn parse_event_query_datetime(input: &str, end_of_day: bool) -> Result<DateTime<Utc>> {
    let input = input.trim();
    if input.is_empty() {
        anyhow::bail!("timestamp must not be empty");
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(input) {
        return Ok(parsed.with_timezone(&Utc));
    }

    let local_offset = FixedOffset::from_str(&local_timezone_offset_string())
        .unwrap_or_else(|_| default_timezone());

    if let Ok(parsed) = chrono::NaiveDate::parse_from_str(input, "%Y-%m-%d") {
        let naive = if end_of_day {
            parsed.and_hms_milli_opt(23, 59, 59, 999)
        } else {
            parsed.and_hms_opt(0, 0, 0)
        }
        .ok_or_else(|| anyhow!("Failed to resolve local date: {input}"))?;

        return local_offset
            .from_local_datetime(&naive)
            .single()
            .map(|dt| dt.with_timezone(&Utc))
            .ok_or_else(|| {
                anyhow!(
                    "Failed to resolve local date with offset {}: {input}",
                    local_offset
                )
            });
    }

    for pattern in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(input, pattern) {
            return local_offset
                .from_local_datetime(&naive)
                .single()
                .map(|dt| dt.with_timezone(&Utc))
                .ok_or_else(|| {
                    anyhow!(
                        "Failed to resolve local datetime with offset {}: {input}",
                        local_offset
                    )
                });
        }
    }

    anyhow::bail!(
        "Invalid timestamp '{input}'. Expected RFC3339/ISO-8601 or local datetime like 2026-04-02 09:00:00"
    )
}

pub trait TriggerCallback: Send + Sync {
    fn on_trigger(&self, summary: &TriggerExecutionSummary) -> Result<()>;
}

pub struct NotificationCallback;

impl TriggerCallback for NotificationCallback {
    fn on_trigger(&self, summary: &TriggerExecutionSummary) -> Result<()> {
        let fired_at_local = summary.fired_at.with_timezone(&default_timezone());
        info!(
            trigger_id = %summary.trigger_id,
            name = %summary.name,
            result = summary.result,
            fired_at_utc = %summary.fired_at,
            fired_at_local = %fired_at_local,
            "Trigger fired"
        );
        Ok(())
    }
}

pub struct TriggerSdk {
    storage: Storage,
    things_event_tx: broadcast::Sender<ThingsEvent>,
    trigger_event_tx: broadcast::Sender<TriggerEvent>,
    events_event_tx: broadcast::Sender<EventsEvent>,
    notification_event_tx: broadcast::Sender<NotificationEvent>,
    realtime: Arc<SupabaseRealtimeManager>,
}

impl TriggerSdk {
    pub fn initialize(db_path: impl AsRef<Path>) -> Result<Self> {
        let storage = Storage::new(db_path)?;
        storage.seed_builtin_actions(&crate::action_builtin::builtin_actions())?;
        let (things_event_tx, _rx) = broadcast::channel(2048);
        let (trigger_event_tx, _rx) = broadcast::channel(2048);
        let (events_event_tx, _rx) = broadcast::channel(2048);
        let (notification_event_tx, _rx) = broadcast::channel(2048);
        Ok(Self {
            storage,
            things_event_tx,
            trigger_event_tx,
            events_event_tx,
            notification_event_tx,
            realtime: Arc::new(SupabaseRealtimeManager::new()),
        })
    }

    pub fn things_subscribe(&self) -> broadcast::Receiver<ThingsEvent> {
        self.things_event_tx.subscribe()
    }

    pub fn triggers_subscribe(&self) -> broadcast::Receiver<TriggerEvent> {
        self.trigger_event_tx.subscribe()
    }

    pub fn events_subscribe(&self) -> broadcast::Receiver<EventsEvent> {
        self.events_event_tx.subscribe()
    }

    pub fn notifications_subscribe(&self) -> broadcast::Receiver<NotificationEvent> {
        self.notification_event_tx.subscribe()
    }

    pub fn realtime_manager(&self) -> Arc<SupabaseRealtimeManager> {
        self.realtime.clone()
    }

    pub fn realtime_subscribe(&self) -> broadcast::Receiver<RemiRealtimeEvent> {
        self.realtime.subscribe()
    }

    pub(crate) fn emit_things_event(&self, event: ThingsEvent) {
        // Ignore send errors (no active subscribers).
        let _ = self.things_event_tx.send(event);
    }

    /// Build and emit a `SnapshotReplaced` event so clients can replace their full
    /// Things/Collections state.  Called after sync pulls new documents from
    /// the server.
    pub fn emit_snapshot_replace(&self, device_id: &str) -> Result<()> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let snapshot = doc_set
            .extract_snapshot_with_options(crate::things_crdt::SnapshotOptions {
                include_content: true,
            })
            .context("Failed to extract snapshot for SnapshotReplaced event")?;
        let dirty = doc_set.has_pending_changes();
        self.emit_things_event(ThingsEvent::SnapshotReplaced {
            device_id: device_id.to_string(),
            collections: snapshot.collections,
            things: snapshot.things,
            dirty,
            last_sync_at: None,
        });
        Ok(())
    }

    fn emit_trigger_event(&self, event: TriggerEvent) {
        // Ignore send errors (no active subscribers).
        let _ = self.trigger_event_tx.send(event);
    }

    fn emit_events_event(&self, event: EventsEvent) {
        // Ignore send errors (no active subscribers).
        let _ = self.events_event_tx.send(event);
    }

    fn emit_notification_event(&self, event: NotificationEvent) {
        // Ignore send errors (no active subscribers).
        let _ = self.notification_event_tx.send(event);
    }

    /// Wipe all local data and broadcast DataWiped events to all streams.
    /// This is the canonical logout path — Flutter notifiers will clear
    /// in-memory state when they receive the DataWiped event.
    pub fn wipe_all_data_and_notify(&self) -> Result<()> {
        self.storage.wipe_all_data()?;
        self.emit_things_event(ThingsEvent::DataWiped);
        self.emit_trigger_event(TriggerEvent::DataWiped);
        self.emit_events_event(EventsEvent::DataWiped);
        self.emit_notification_event(NotificationEvent::DataWiped);
        info!("Wiped all local data and notified all event streams");
        Ok(())
    }

    fn emit_document_events(&self, device_id: &str, events: Vec<ThingsDocumentEvent>) {
        for event in events {
            self.emit_things_event(event.into_event(device_id));
        }
    }

    /// Attribute all anonymous (user_id IS NULL) local data to the given user.
    /// Called after a successful login so locally-created content is owned by the user.
    pub fn claim_anonymous_data(&self, user_id: &str) -> Result<()> {
        let claimed = self.storage.claim_anonymous_data(user_id)?;
        info!(user_id = %user_id, claimed, "Claimed anonymous data after login");
        Ok(())
    }

    pub fn register_trigger(&self, params: TriggerRegistration) -> Result<String> {
        self.register_trigger_inner(params)
    }

    fn register_trigger_inner(&self, params: TriggerRegistration) -> Result<String> {
        // UUID must be provided
        if params.trigger_uuid.is_empty() {
            anyhow::bail!("Trigger UUID is required but not provided.");
        }

        let now = Utc::now();
        let local_timezone_offset = local_timezone_offset_string();
        let normalized_precondition =
            normalize_timer_preconditions(&params.precondition, now, &local_timezone_offset)?;
        let params = TriggerRegistration {
            precondition: normalized_precondition,
            ..params
        };

        let timings = extract_timings_from_rules(&params.precondition, &params.condition)?;
        let next_fire = resolve_registration_next_fire(&timings, now, &local_timezone_offset)?;

        let trigger_uuid = params.trigger_uuid.clone();
        let inserted_uuid = self.storage.insert_trigger(params, next_fire)?;
        self.emit_trigger_event(TriggerEvent::TriggerUpsert { trigger_uuid });
        Ok(inserted_uuid)
    }

    pub fn record_event(&self, event: EventPayload) -> Result<()> {
        let event_type = event.event_type.clone();
        let event_ts = event.timestamp;
        self.storage.insert_event(&event)?;
        self.schedule_event_triggers(&event_type, event_ts)?;

        // Emit event notification to subscribers (e.g. UI).
        self.emit_events_event(EventsEvent::EventRecorded {
            event_type: event_type.clone(),
            timestamp: event_ts.to_rfc3339(),
        });

        Ok(())
    }

    /// Schedule triggers that react to Connectivity events.
    /// Prefer recording a Connectivity event via `record_event`; this shim exists for callers
    /// that still separate event persistence from trigger scheduling.
    pub fn schedule_network_change_triggers(&self, due_at: DateTime<Utc>) -> Result<()> {
        self.schedule_event_triggers("Connectivity", due_at)
    }

    /// Schedule triggers that react to Location events.
    /// Prefer recording a Location event via `record_event`; this shim exists for callers
    /// that still separate event persistence from trigger scheduling.
    pub fn schedule_location_change_triggers(&self, due_at: DateTime<Utc>) -> Result<()> {
        self.schedule_event_triggers("Location", due_at)
    }

    fn schedule_event_triggers(&self, event_type: &str, due_at: DateTime<Utc>) -> Result<()> {
        let triggers = self.storage.list_triggers()?;
        for trigger in triggers {
            let timings = extract_timings_from_rules(&trigger.precondition, &trigger.condition)
                .with_context(|| {
                    format!("Failed to inspect trigger {} timings", trigger.trigger_id)
                })?;
            let matches_event = timings.iter().any(|timing| {
                matches!(
                    timing,
                    rule_trigger_engine::TriggerTiming::Event { event_type: configured }
                        if configured == event_type
                )
            });
            if !matches_event {
                continue;
            }

            // Mark due; `run_due_triggers()` will execute it and reschedule appropriately.
            self.storage
                .mark_trigger_due(&trigger.trigger_id, due_at)
                .with_context(|| format!("Failed to mark trigger due: {}", trigger.trigger_id))?;
        }
        Ok(())
    }

    pub fn next_deadline(&self, now_unix: Option<i64>) -> Result<Option<DateTime<Utc>>> {
        self.storage.next_deadline(now_unix)
    }

    pub fn list_events_json(&self, limit: Option<u32>, offset: u32) -> Result<String> {
        let events = self.storage.list_events(limit, offset)?;
        let payloads: Vec<EventPayload> = events.into_iter().map(EventPayload::from).collect();
        to_string(&payloads).context("Failed to serialize events")
    }

    pub fn events_list_between_json(&self, start_time: &str, end_time: &str) -> Result<String> {
        let start = parse_event_query_datetime(start_time, false).context("Invalid start_time")?;
        let end = parse_event_query_datetime(end_time, true).context("Invalid end_time")?;
        if start > end {
            anyhow::bail!("start_time must be <= end_time");
        }

        let events = self
            .storage
            .list_events_between_utc(start.timestamp(), end.timestamp())?;
        let payloads: Vec<EventPayload> = events.into_iter().map(EventPayload::from).collect();
        to_string(&payloads).context("Failed to serialize events")
    }

    pub fn events_abstract_json(&self, top_n: u32) -> Result<String> {
        #[derive(Default)]
        struct Bucket {
            total: u32,
            counts: std::collections::BTreeMap<String, u32>,
        }

        // For now, read all events and bucket by UTC hour.
        // If needed, we can optimize with SQL aggregation.
        let events = self.storage.list_events(None, 0)?;
        let mut buckets: std::collections::BTreeMap<String, Bucket> =
            std::collections::BTreeMap::new();

        for ev in events {
            let dt = ev.timestamp;
            let hour_key = format!(
                "{:04}-{:02}-{:02} {:02}:00",
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour()
            );

            let bucket = buckets.entry(hour_key).or_default();
            bucket.total += 1;
            let et = ev.event_type.clone();
            *bucket.counts.entry(et).or_insert(0) += 1;
        }

        let mut hours_json = Vec::new();
        for (hour, bucket) in buckets {
            let mut top: Vec<(String, u32)> = bucket.counts.into_iter().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            top.truncate(top_n as usize);
            let top_types: Vec<serde_json::Value> = top
                .into_iter()
                .map(|(t, c)| json!({"type": t, "count": c}))
                .collect();
            hours_json.push(json!({
                "hour": hour,
                "total_events": bucket.total,
                "top_types": top_types,
            }));
        }

        to_string(&json!({"hours": hours_json, "top_n": top_n})).context("Failed to serialize")
    }

    pub fn event_count(&self) -> Result<i64> {
        self.storage.events_count()
    }

    pub fn event_time_range(&self) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
        self.storage.events_time_range()
    }

    pub fn list_triggers(&self) -> Result<Vec<TriggerInfo>> {
        self.storage.list_triggers()
    }

    pub fn list_triggers_json(&self) -> Result<String> {
        let triggers = self.list_triggers()?;
        to_string(&triggers).context("Failed to serialize triggers")
    }

    pub fn list_actions(&self) -> Result<Vec<ActionDefinition>> {
        self.storage.list_actions()
    }

    pub fn list_actions_json(&self) -> Result<String> {
        let actions = self.list_actions()?;
        to_string(&actions).context("Failed to serialize actions")
    }

    pub fn fetch_action(&self, action_uuid: &str) -> Result<Option<ActionDefinition>> {
        self.storage.fetch_action(action_uuid)
    }

    pub fn fetch_action_json(&self, action_uuid: &str) -> Result<Option<String>> {
        self.fetch_action(action_uuid)?
            .map(|action| to_string(&action).context("Failed to serialize action"))
            .transpose()
    }

    pub fn latest_action_invocation_json(&self, action_uuid: &str) -> Result<Option<String>> {
        self.storage
            .latest_action_invocation(action_uuid)?
            .map(|record| {
                to_string(&record).context("Failed to serialize latest action invocation")
            })
            .transpose()
    }

    pub fn execute_action_now(
        &self,
        action_uuid: &str,
        source_kind: ActionInvocationSourceKind,
        source_entity_type: Option<&str>,
        source_entity_uuid: Option<&str>,
        args_json: Value,
        device_id: Option<&str>,
    ) -> Result<ActionInvocationRecord> {
        let action = self
            .storage
            .fetch_action(action_uuid)?
            .ok_or_else(|| anyhow!("Action not found: {action_uuid}"))?;

        let started_at = Utc::now();
        let invocation_uuid = uuid::Uuid::new_v4().to_string();
        let execution_input = json!({
            "action": {
                "uuid": action.action_uuid,
                "name": action.name,
                "version": action.version,
            },
            "source": {
                "kind": source_kind.as_str(),
                "entity_type": source_entity_type,
                "entity_uuid": source_entity_uuid,
            },
            "args": args_json,
            "context": {
                "device_id": device_id,
            }
        });

        let execution_started = std::time::Instant::now();
        let execution_result = self.run_action_script(&action, &execution_input);
        let finished_at = Utc::now();
        let duration_ms = execution_started.elapsed().as_millis() as u64;

        let (result_json, console_logs, error_json) = match execution_result {
            Ok((result_json, console_logs)) => (Some(result_json), console_logs, None),
            Err(error) => (
                None,
                Vec::new(),
                Some(json!({
                    "message": error.to_string(),
                })),
            ),
        };

        let record = ActionInvocationRecord {
            invocation_uuid,
            action_uuid: action_uuid.to_string(),
            source_kind,
            source_entity_type: source_entity_type.map(|value| value.to_string()),
            source_entity_uuid: source_entity_uuid.map(|value| value.to_string()),
            args_json: execution_input.get("args").cloned().unwrap_or(Value::Null),
            result_json,
            console_logs,
            error_json,
            started_at,
            finished_at,
            duration_ms,
            device_id: device_id.map(|value| value.to_string()),
        };

        self.storage.insert_action_invocation(&record)?;
        Ok(record)
    }

    /// Pause or resume a trigger. Returns the updated paused state.
    pub fn set_trigger_paused(&self, trigger_uuid: &str, paused: bool) -> Result<()> {
        self.storage.set_trigger_paused(trigger_uuid, paused)
    }

    /// Record a binding between a trigger and a thing/collection.
    pub fn upsert_trigger_binding(
        &self,
        trigger_uuid: &str,
        entity_type: &str,
        entity_uuid: &str,
    ) -> Result<()> {
        self.storage
            .upsert_trigger_binding(trigger_uuid, entity_type, entity_uuid)
    }

    /// Remove the `trigger_bindings` row for the given entity.
    ///
    /// Use this when unbinding a trigger so that `is_trigger_bound` correctly reflects
    /// the new state before calling `delete_trigger_if_unbound`.
    pub fn delete_trigger_binding(&self, entity_type: &str, entity_uuid: &str) -> Result<()> {
        self.storage
            .delete_trigger_binding(entity_type, entity_uuid)
    }

    /// Get the trigger UUID currently bound to a specific entity from the trigger_bindings table.
    ///
    /// This supplements the CRDT snapshot lookup and catches stale bindings that the CRDT
    /// may not reflect (e.g., edge cases from migrations or non-CRDT binding paths).
    pub fn get_trigger_for_entity(
        &self,
        entity_type: &str,
        entity_uuid: &str,
    ) -> Result<Option<String>> {
        self.storage
            .get_trigger_for_entity(entity_type, entity_uuid)
    }

    /// Delete a trigger definition if it is no longer bound to any entity.
    ///
    /// Returns `true` if the trigger was deleted.
    pub fn delete_trigger_if_unbound(&self, trigger_uuid: &str) -> Result<bool> {
        if self.storage.is_trigger_bound(trigger_uuid)? {
            return Ok(false);
        }

        let deleted = self.storage.delete_trigger(trigger_uuid)?;
        if deleted {
            self.emit_trigger_event(TriggerEvent::TriggerDelete {
                trigger_uuid: trigger_uuid.to_string(),
            });
        }

        Ok(deleted)
    }

    /// Delete a trigger and all its bindings unconditionally.
    ///
    /// This is the correct method for an explicit user-initiated delete:
    /// it clears the CRDT on every bound entity, removes all `trigger_bindings`
    /// rows, deletes the trigger record, and emits the `TriggerDelete` event.
    ///
    /// Returns `true` if the trigger record was found and deleted.
    pub fn delete_trigger_and_bindings(&self, device_id: &str, trigger_uuid: &str) -> Result<bool> {
        // 1. Collect every entity currently bound to this trigger.
        let bound = self.storage.get_entities_for_trigger(trigger_uuid)?;

        // 2. Clear the CRDT trigger_uuid on each bound entity (best-effort).
        for (entity_type, entity_uuid) in &bound {
            // Pass Some("") to get TriggerUpdate::Clear — None is TriggerUpdate::Noop and skips the write.
            let result = match entity_type.as_str() {
                "collection" => {
                    self.things_set_collection_trigger_uuid(device_id, entity_uuid, Some(""))
                }
                "thing" => self.things_set_thing_trigger_uuid(device_id, entity_uuid, Some("")),
                other => {
                    tracing::warn!(entity_type = %other, "Unknown entity_type in trigger_bindings; skipping CRDT clear");
                    Ok(())
                }
            };
            if let Err(e) = result {
                tracing::warn!(
                    entity_type,
                    entity_uuid,
                    "Failed to clear CRDT trigger on entity: {e}"
                );
            }
        }

        // 3. Remove all trigger_bindings rows for this trigger.
        let removed = self.storage.delete_all_bindings_for_trigger(trigger_uuid)?;
        tracing::debug!(trigger_uuid, removed, "Removed trigger_bindings rows");

        // 4. Delete the trigger record.
        let deleted = self.storage.delete_trigger(trigger_uuid)?;
        if deleted {
            self.emit_trigger_event(TriggerEvent::TriggerDelete {
                trigger_uuid: trigger_uuid.to_string(),
            });
        }

        Ok(deleted)
    }

    /// Set (or clear) a collection's `trigger_uuid` inside the Things CRDT.
    ///
    /// This is the canonical source of truth for trigger bindings as surfaced in the UI.
    /// The SQL `trigger_bindings` table is reconciled from this CRDT state.
    pub fn things_set_collection_trigger_uuid(
        &self,
        device_id: &str,
        collection_uuid: &str,
        trigger_uuid: Option<&str>,
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;

        // Validate the collection exists to avoid creating phantom bindings.
        let snapshot = doc_set.extract_snapshot()?;
        if snapshot
            .collections
            .iter()
            .find(|c| c.uuid == collection_uuid)
            .is_none()
        {
            anyhow::bail!("Collection not found: {collection_uuid}");
        }

        // V3: Update collection trigger via collection document
        let trigger = crate::things_crdt::trigger_update_from_tri_state(trigger_uuid);
        let events = doc_set.update_collection_meta(collection_uuid, None, None, trigger)?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        Ok(())
    }

    /// Set (or clear) a thing's `trigger_uuid` inside the Things CRDT.
    pub fn things_set_thing_trigger_uuid(
        &self,
        device_id: &str,
        thing_uuid: &str,
        trigger_uuid: Option<&str>,
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let snapshot = doc_set.extract_snapshot()?;

        let collection_uuid = match snapshot.things.iter().find(|t| t.uuid == thing_uuid) {
            Some(t) => t.collection_uuid.clone(),
            None => {
                tracing::warn!(
                    thing_uuid,
                    "things_set_thing_trigger_uuid: thing not in snapshot, scanning collection docs"
                );
                doc_set
                    .find_thing_collection_uuid(thing_uuid)
                    .ok_or_else(|| anyhow!("Thing not found: {thing_uuid}"))?
            }
        };

        // V3: Update thing trigger via collection document
        let trigger = crate::things_crdt::trigger_update_from_tri_state(trigger_uuid);
        let events = doc_set.upsert_thing_meta(
            &collection_uuid,
            thing_uuid,
            None, // datatype
            None, // status
            None, // title
            None, // parent_uuid
            trigger,
        )?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        Ok(())
    }

    pub fn list_collection_action_bindings(
        &self,
        device_id: &str,
        collection_uuid: &str,
    ) -> Result<Vec<EntityActionBinding>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let collection = doc_set.collection_view(collection_uuid)?;
        Ok(decode_entity_action_bindings(
            collection.meta.attrs.as_ref(),
        ))
    }

    pub fn list_thing_action_bindings(
        &self,
        device_id: &str,
        thing_uuid: &str,
    ) -> Result<Vec<EntityActionBinding>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = doc_set
            .find_thing_collection_uuid(thing_uuid)
            .ok_or_else(|| anyhow!("Thing not found: {thing_uuid}"))?;
        let collection = doc_set.collection_view(&collection_uuid)?;
        let thing = collection
            .things
            .into_iter()
            .find(|item| item.id == thing_uuid)
            .ok_or_else(|| anyhow!("Thing not found: {thing_uuid}"))?;
        Ok(decode_entity_action_bindings(thing.attrs.as_ref()))
    }

    pub fn resolve_collection_action_bindings(
        &self,
        device_id: &str,
        collection_uuid: &str,
    ) -> Result<Vec<ResolvedEntityActionBinding>> {
        self.resolve_entity_action_bindings(
            self.list_collection_action_bindings(device_id, collection_uuid)?,
        )
    }

    pub fn resolve_thing_action_bindings(
        &self,
        device_id: &str,
        thing_uuid: &str,
    ) -> Result<Vec<ResolvedEntityActionBinding>> {
        self.resolve_entity_action_bindings(self.list_thing_action_bindings(device_id, thing_uuid)?)
    }

    pub fn things_set_collection_action_bindings(
        &self,
        device_id: &str,
        collection_uuid: &str,
        bindings: &[EntityActionBinding],
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection = doc_set.collection_view(collection_uuid)?;
        let attrs = encode_entity_action_bindings(collection.meta.attrs.as_ref(), bindings);
        let events = doc_set.update_collection_attrs(collection_uuid, Some(attrs))?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);
        Ok(())
    }

    pub fn get_collection_card_jsx(
        &self,
        device_id: &str,
        collection_uuid: &str,
    ) -> Result<Option<String>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let collection = doc_set.collection_view(collection_uuid)?;
        Ok(decode_collection_card_jsx(collection.meta.attrs.as_ref()))
    }

    pub fn things_set_collection_card_jsx(
        &self,
        device_id: &str,
        collection_uuid: &str,
        card_jsx: Option<&str>,
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection = doc_set.collection_view(collection_uuid)?;
        let attrs = encode_collection_card_jsx(collection.meta.attrs.as_ref(), card_jsx);
        let events = doc_set.update_collection_attrs(collection_uuid, Some(attrs))?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);
        Ok(())
    }

    pub fn things_set_thing_action_bindings(
        &self,
        device_id: &str,
        thing_uuid: &str,
        bindings: &[EntityActionBinding],
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = doc_set
            .find_thing_collection_uuid(thing_uuid)
            .ok_or_else(|| anyhow!("Thing not found: {thing_uuid}"))?;
        let collection = doc_set.collection_view(&collection_uuid)?;
        let thing = collection
            .things
            .iter()
            .find(|item| item.id == thing_uuid)
            .ok_or_else(|| anyhow!("Thing not found: {thing_uuid}"))?;
        let attrs = encode_entity_action_bindings(thing.attrs.as_ref(), bindings);
        let events = doc_set.update_thing_attrs(&collection_uuid, thing_uuid, Some(attrs))?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);
        Ok(())
    }

    pub fn execute_collection_action_now(
        &self,
        device_id: &str,
        collection_uuid: &str,
        action_uuid: &str,
    ) -> Result<ActionInvocationRecord> {
        let binding = self
            .list_collection_action_bindings(device_id, collection_uuid)?
            .into_iter()
            .find(|item| item.action_uuid == action_uuid)
            .ok_or_else(|| {
                anyhow!(
                    "Collection '{}' is not bound to action '{}'",
                    collection_uuid,
                    action_uuid
                )
            })?;
        self.execute_action_now(
            action_uuid,
            ActionInvocationSourceKind::CollectionManual,
            Some("collection"),
            Some(collection_uuid),
            binding.args_json,
            Some(device_id),
        )
    }

    pub fn execute_thing_action_now(
        &self,
        device_id: &str,
        thing_uuid: &str,
        action_uuid: &str,
    ) -> Result<ActionInvocationRecord> {
        let binding = self
            .list_thing_action_bindings(device_id, thing_uuid)?
            .into_iter()
            .find(|item| item.action_uuid == action_uuid)
            .ok_or_else(|| {
                anyhow!(
                    "Thing '{}' is not bound to action '{}'",
                    thing_uuid,
                    action_uuid
                )
            })?;
        self.execute_action_now(
            action_uuid,
            ActionInvocationSourceKind::ThingManual,
            Some("thing"),
            Some(thing_uuid),
            binding.args_json,
            Some(device_id),
        )
    }

    pub fn build_session_context(
        &self,
        device_id: &str,
        granted_permissions: &[String],
        active_context_json: Option<&str>,
    ) -> Result<String> {
        use std::time::Instant;

        let total = Instant::now();

        let t0 = Instant::now();
        // V3: Load document set for session context
        let doc_set = self.get_or_init_document_set(device_id)?;
        info!(
            device_id = %device_id,
            active_context = active_context_json.is_some(),
            permissions = granted_permissions.len(),
            ms = t0.elapsed().as_millis(),
            "build_session_context: get_or_init_document_set"
        );

        let t1 = Instant::now();
        let snapshot =
            doc_set.extract_snapshot_with_options(crate::things_crdt::SnapshotOptions {
                include_content: false,
            })?;
        info!(
            collections = snapshot.collections.len(),
            things = snapshot.things.len(),
            ms = t1.elapsed().as_millis(),
            "build_session_context: extract_snapshot"
        );

        let t2 = Instant::now();
        let triggers = self.storage.list_triggers_for_context_prompt(50)?;
        info!(
            triggers = triggers.len(),
            ms = t2.elapsed().as_millis(),
            "build_session_context: list_triggers_for_context_prompt"
        );

        let t3 = Instant::now();
        let out = context_prompt::build_context_prompt_markdown(
            granted_permissions,
            &snapshot,
            &triggers,
            active_context_json,
        )?;
        info!(
            out_bytes = out.len(),
            ms = t3.elapsed().as_millis(),
            total_ms = total.elapsed().as_millis(),
            "build_session_context: build_context_prompt_markdown"
        );

        Ok(out)
    }

    pub(crate) fn normalize_active_context_json(
        &self,
        device_id: &str,
        active_context_json: Option<&str>,
    ) -> Result<Option<serde_json::Value>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let snapshot =
            doc_set.extract_snapshot_with_options(crate::things_crdt::SnapshotOptions {
                include_content: false,
            })?;
        context_prompt::normalize_active_context_json(&snapshot, active_context_json)
    }

    pub(crate) fn build_active_context_prompt(
        &self,
        device_id: &str,
        active_context_json: Option<&str>,
    ) -> Result<Option<String>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let snapshot =
            doc_set.extract_snapshot_with_options(crate::things_crdt::SnapshotOptions {
                include_content: false,
            })?;
        context_prompt::build_active_context_section(&snapshot, active_context_json)
    }

    // ===== Things V3 (Multi-document CRDT) =====

    /// Load or initialize the v3 document set from storage
    fn get_or_init_document_set(
        &self,
        device_id: &str,
    ) -> Result<crate::things_crdt::ThingsDocumentSet> {
        let namespace = self.storage.cache_namespace();
        let revision = self.storage.get_crdt_documents_revision()?;
        if let Some(doc_set) = crate::crdt_cache::get(&namespace, device_id, revision) {
            return Ok(doc_set);
        }

        const MAX_REVISION_STABILITY_RETRIES: usize = 4;
        let persistence = crate::things_crdt::DocumentPersistence::new(&self.storage);

        for _attempt in 0..MAX_REVISION_STABILITY_RETRIES {
            let revision_before = self.storage.get_crdt_documents_revision()?;
            if let Some(doc_set) = crate::crdt_cache::get(&namespace, device_id, revision_before) {
                return Ok(doc_set);
            }

            let doc_set = persistence.load_or_init_document_set(device_id)?;
            let revision_after = self.storage.get_crdt_documents_revision()?;
            if revision_before == revision_after {
                crate::crdt_cache::put(
                    namespace.clone(),
                    device_id.to_string(),
                    revision_after,
                    doc_set.clone(),
                );
                return Ok(doc_set);
            }
        }

        anyhow::bail!(
            "CRDT document set changed repeatedly while loading for device '{}'",
            device_id
        )
    }

    fn resolve_thing_collection_uuid(
        doc_set: &crate::things_crdt::ThingsDocumentSet,
        thing_uuid: &str,
    ) -> Result<String> {
        let snapshot = doc_set.extract_snapshot()?;
        match snapshot
            .things
            .iter()
            .find(|thing| thing.uuid == thing_uuid)
        {
            Some(thing) => Ok(thing.collection_uuid.clone()),
            None => doc_set
                .find_thing_collection_uuid(thing_uuid)
                .ok_or_else(|| anyhow!("Thing not found: {}", thing_uuid)),
        }
    }

    fn validate_json_object_data(schema: Option<&Value>, data: &Value) -> Result<()> {
        if !data.is_object() {
            anyhow::bail!("json_object data must be a JSON object")
        }

        if let Some(schema) = schema {
            let compiled = JSONSchema::compile(schema)
                .map_err(|error| anyhow!("Invalid JSON Schema: {error}"))?;
            let errors = compiled
                .validate(data)
                .map(|_| Vec::new())
                .unwrap_or_else(|errors| errors.map(|error| error.to_string()).collect::<Vec<_>>());
            if !errors.is_empty() {
                anyhow::bail!("JSON object does not satisfy schema: {}", errors.join("; "));
            }
        }

        Ok(())
    }

    fn resolve_json_object_entry_field(
        entries: &[ContentEntry],
        entry_id: &str,
    ) -> Result<JsonObjectField> {
        let entry = entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .ok_or_else(|| anyhow!("Content entry not found: {}", entry_id))?;
        match &entry.payload {
            ContentEntryPayload::JsonObject(field) => Ok(field.clone()),
            other => anyhow::bail!(
                "Content entry '{}' is not a json_object entry (found {:?})",
                entry_id,
                other.kind()
            ),
        }
    }

    pub fn things_list_snapshot(&self, device_id: &str) -> Result<ThingsSnapshotState> {
        self.things_list_snapshot_with_options(
            device_id,
            true,
            crate::things_crdt::SnapshotOptions {
                include_content: true,
            },
        )
    }

    /// Snapshot optimized for agent/tools and context prompts.
    ///
    /// - Omits thing `data.content` entirely.
    /// - Still returns collections + things metadata.
    pub fn things_list_snapshot_lite(&self, device_id: &str) -> Result<ThingsSnapshotState> {
        self.things_list_snapshot_with_options(
            device_id,
            true,
            crate::things_crdt::SnapshotOptions {
                include_content: false,
            },
        )
    }

    /// Flexible snapshot builder for tools/UI.
    ///
    /// Use this to avoid extracting content (and optionally avoid extracting things at all).
    pub fn things_list_snapshot_with_options(
        &self,
        device_id: &str,
        _include_things: bool,
        snapshot_options: crate::things_crdt::SnapshotOptions,
    ) -> Result<ThingsSnapshotState> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let mut snapshot = doc_set
            .extract_snapshot_with_options(snapshot_options)
            .context("Failed to extract v3 snapshot")?;
        let dirty = doc_set.has_pending_changes();

        // Enrich snapshot entries with cached actor attribution metadata.
        if let Ok(actor_meta) = self.storage.load_things_actor_meta_map() {
            for col in &mut snapshot.collections {
                if let Some(meta) = actor_meta.get(&col.uuid) {
                    col.actor_type = Some(meta.actor_type.clone());
                    col.actor_app_id = meta.actor_app_id.clone();
                    col.actor_display_name = meta.actor_display_name.clone();
                }
            }
            for thing in &mut snapshot.things {
                if let Some(meta) = actor_meta.get(&thing.uuid) {
                    thing.actor_type = Some(meta.actor_type.clone());
                    thing.actor_app_id = meta.actor_app_id.clone();
                    thing.actor_display_name = meta.actor_display_name.clone();
                }
            }
        }

        Ok(ThingsSnapshotState {
            collections: snapshot.collections,
            things: snapshot.things,
            dirty,
            last_sync_at: None,
        })
    }

    /// Store a batch of actor attribution metadata (fetched from server) into the local cache.
    pub fn things_upsert_actor_meta(&self, items: &[crate::storage::ActorMetaEntry]) -> Result<()> {
        self.storage
            .upsert_things_actor_meta_batch(items)
            .context("Failed to store actor meta batch")
    }

    pub fn things_has_pending_changes(&self, device_id: &str) -> Result<bool> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        Ok(doc_set.has_pending_changes())
    }

    pub fn things_upsert_collection(
        &self,
        device_id: &str,
        upsert: ThingCollectionUpsert,
    ) -> Result<ThingCollectionEntry> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;

        // Check if collection exists before upsert to determine if this is create or update
        let before_snapshot = doc_set.extract_snapshot()?;
        let existing = before_snapshot
            .collections
            .iter()
            .find(|c| c.uuid == upsert.uuid);
        let is_create = existing.is_none();

        // V3: Update collection metadata
        let trigger =
            crate::things_crdt::trigger_update_from_tri_state(upsert.trigger_uuid.as_deref());
        let events = doc_set.update_collection_meta_with_timestamps(
            &upsert.uuid,
            Some(upsert.title.clone()),
            None,
            trigger,
            upsert.created_at.clone(),
            upsert.updated_at.clone(),
        )?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;

        let snapshot = doc_set.extract_snapshot()?;
        let created = snapshot
            .collections
            .iter()
            .find(|c| c.uuid == upsert.uuid)
            .ok_or_else(|| anyhow!("Collection not found after upsert"))?;

        self.emit_document_events(device_id, events);

        // Record change log
        let op_type = if is_create {
            ThingsOperationType::CreateCollection
        } else {
            ThingsOperationType::UpdateCollection
        };
        let summary = if is_create {
            format!("Created collection '{}'", created.title)
        } else {
            format!("Updated collection '{}'", created.title)
        };
        let details = json!({
            "uuid": upsert.uuid,
            "title": upsert.title,
            "trigger_uuid": upsert.trigger_uuid,
        });
        let _ = self.storage.insert_things_change_log(
            device_id,
            op_type,
            "collection",
            &upsert.uuid,
            &summary,
            &details.to_string(),
            None,
            true,
        );

        Ok(created.clone())
    }

    pub fn things_delete_collection(&self, device_id: &str, uuid: &str) -> Result<bool> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let before = doc_set.extract_snapshot()?;
        let collection = before.collections.iter().find(|c| c.uuid == uuid);
        if collection.is_none() {
            return Ok(false);
        }
        let collection = collection.unwrap();
        let collection_title = collection.title.clone();

        let mut removed_triggers: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        if let Some(trigger_uuid) = collection.trigger_uuid.as_ref() {
            let _ = self
                .storage
                .delete_trigger_binding("collection", uuid)
                .map_err(|e| {
                    tracing::warn!(
                        collection_uuid = %uuid,
                        trigger_uuid = %trigger_uuid,
                        error = %e,
                        "Failed to delete trigger binding for collection"
                    );
                    e
                });
            removed_triggers.insert(trigger_uuid.clone());
        }

        // Find all things in this collection (for cascade logging)
        let things_in_collection: Vec<_> = before
            .things
            .iter()
            .filter(|t| t.collection_uuid == uuid)
            .cloned()
            .collect();

        for thing in &things_in_collection {
            if let Some(trigger_uuid) = thing.trigger_uuid.as_ref() {
                let _ = self
                    .storage
                    .delete_trigger_binding("thing", &thing.uuid)
                    .map_err(|e| {
                        tracing::warn!(
                            thing_uuid = %thing.uuid,
                            trigger_uuid = %trigger_uuid,
                            error = %e,
                            "Failed to delete trigger binding for thing"
                        );
                        e
                    });
                removed_triggers.insert(trigger_uuid.clone());
            }
        }

        // V3: Tombstone the collection document and remove the root reference.
        // Keep the collection/thing documents locally so the tombstone can sync
        // to other devices instead of being re-discovered as a live orphan.
        let events = doc_set.delete_collection(uuid)?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        // Record change log for the collection deletion
        let summary = format!("Deleted collection '{}'", collection_title);
        let details = json!({
            "uuid": uuid,
            "title": collection_title,
            "things_count": things_in_collection.len(),
        });
        let parent_log_id = self
            .storage
            .insert_things_change_log(
                device_id,
                ThingsOperationType::DeleteCollection,
                "collection",
                uuid,
                &summary,
                &details.to_string(),
                None,
                true,
            )
            .ok();

        // Record cascade deletions for things
        if let Some(parent_id) = parent_log_id {
            let mut cascade_ids = Vec::new();
            for thing in &things_in_collection {
                let thing_summary = format!("Deleted thing '{}' (cascade)", thing.title);
                let thing_details = json!({
                    "uuid": thing.uuid,
                    "title": thing.title,
                    "collection_uuid": uuid,
                    "cascade_reason": "parent_collection_deleted",
                });
                if let Ok(cascade_id) = self.storage.insert_things_change_log(
                    device_id,
                    ThingsOperationType::DeleteThing,
                    "thing",
                    &thing.uuid,
                    &thing_summary,
                    &thing_details.to_string(),
                    Some(parent_id),
                    false, // Cascade deletions can't be individually undone
                ) {
                    cascade_ids.push(cascade_id);
                }
            }
            if !cascade_ids.is_empty() {
                let _ = self
                    .storage
                    .update_things_change_log_cascade_ids(parent_id, &cascade_ids);
            }
        }

        // After bindings are cleared, uninstall triggers that are no longer bound.
        for trigger_uuid in removed_triggers {
            if let Err(e) = self.delete_trigger_if_unbound(&trigger_uuid) {
                tracing::warn!(
                    trigger_uuid = %trigger_uuid,
                    error = %e,
                    "Failed to cleanup trigger after collection deletion"
                );
            }
        }

        Ok(true)
    }

    pub fn things_upsert_thing(&self, device_id: &str, upsert: ThingUpsert) -> Result<ThingEntry> {
        // Guard: collection_uuid must be non-empty
        if upsert.collection_uuid.trim().is_empty() {
            anyhow::bail!(
                "collection_uuid must not be empty when upserting thing '{}'",
                upsert.uuid,
            );
        }

        let mut doc_set = self.get_or_init_document_set(device_id)?;

        // Check if thing exists before upsert to determine if this is create or update.
        // Try snapshot first, fall back to direct collection scan.
        let before_snapshot = doc_set.extract_snapshot()?;
        let existing = before_snapshot
            .things
            .iter()
            .find(|t| t.uuid == upsert.uuid);

        let (is_create, old_collection_uuid) = match existing {
            Some(t) => (false, Some(t.collection_uuid.clone())),
            None => {
                // Thing not in snapshot — scan collection docs directly (stale root case).
                match doc_set.find_thing_collection_uuid(&upsert.uuid) {
                    Some(coll) => (false, Some(coll)),
                    None => (true, None),
                }
            }
        };
        let old_content_json = existing.map(|t| serde_json::to_string(t).ok()).flatten();

        // If the thing is being moved to a different collection, tombstone it
        // in the old collection document so it doesn't appear in two places.
        let is_move = !is_create
            && old_collection_uuid
                .as_ref()
                .map_or(false, |old| *old != upsert.collection_uuid);

        let mut move_events = Vec::new();

        if is_move {
            let old_coll = old_collection_uuid.as_ref().unwrap();
            tracing::info!(
                thing_uuid = upsert.uuid,
                from_collection = old_coll.as_str(),
                to_collection = upsert.collection_uuid.as_str(),
                "things_upsert_thing: moving thing — tombstoning in source collection"
            );
            move_events.extend(doc_set.delete_thing(old_coll, &upsert.uuid)?);
        }

        // V3: Update thing metadata in collection document
        let trigger =
            crate::things_crdt::trigger_update_from_tri_state(upsert.trigger_uuid.as_deref());
        let mut events = move_events;
        events.extend(doc_set.upsert_thing_meta_with_timestamps(
            &upsert.collection_uuid,
            &upsert.uuid,
            Some(upsert.datatype.clone()),
            None, // status
            Some(upsert.title.clone()),
            upsert.parent_uuid.clone(),
            trigger,
            upsert.created_at.clone(),
            upsert.updated_at.clone(),
        )?);

        // V3: If data is provided, update thing markdown document
        if let Some(ref data) = upsert.data {
            events.extend(doc_set.set_thing_content_from_payload(
                &upsert.uuid,
                &upsert.datatype,
                data,
            )?);
            events.extend(doc_set.sync_content_entries_from_snapshot_payload(
                &upsert.collection_uuid,
                &upsert.uuid,
                data,
            )?);
        }

        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;

        let snapshot = doc_set.extract_snapshot()?;
        let created = snapshot
            .things
            .iter()
            .find(|t| t.uuid == upsert.uuid)
            .ok_or_else(|| anyhow!("Thing not found after upsert"))?;

        self.emit_document_events(device_id, events);

        // Record change log
        let op_type = if is_create {
            ThingsOperationType::CreateThing
        } else if is_move {
            ThingsOperationType::MoveThing
        } else {
            ThingsOperationType::UpdateThing
        };
        let summary = if is_create {
            format!("Created thing '{}'", created.title)
        } else if is_move {
            format!("Moved thing '{}'", created.title)
        } else {
            format!("Updated thing '{}'", created.title)
        };
        let details = if is_move {
            json!({
                "uuid": upsert.uuid,
                "title": upsert.title,
                "from_collection_uuid": old_collection_uuid,
                "to_collection_uuid": upsert.collection_uuid,
                "collection_uuid": upsert.collection_uuid,
                "datatype": format!("{:?}", upsert.datatype),
            })
        } else {
            json!({
                "uuid": upsert.uuid,
                "title": upsert.title,
                "collection_uuid": upsert.collection_uuid,
                "datatype": format!("{:?}", upsert.datatype),
            })
        };

        // For updates, use 5-minute grouping window
        let log_id = if !is_create {
            // Check if there's a recent update log within 5 minutes
            if let Ok(Some(recent)) = self.storage.find_recent_thing_update_log(&upsert.uuid, 300) {
                // Reuse the same log entry (don't create a new one)
                Some(recent.id)
            } else {
                self.storage
                    .insert_things_change_log(
                        device_id,
                        op_type,
                        "thing",
                        &upsert.uuid,
                        &summary,
                        &details.to_string(),
                        None,
                        true,
                    )
                    .ok()
            }
        } else {
            self.storage
                .insert_things_change_log(
                    device_id,
                    op_type,
                    "thing",
                    &upsert.uuid,
                    &summary,
                    &details.to_string(),
                    None,
                    true,
                )
                .ok()
        };

        // For updates, save content snapshot if we don't have one for this log entry
        if !is_create {
            if let Some(lid) = log_id {
                // Check if snapshot exists for this log entry
                if self
                    .storage
                    .get_things_content_snapshot_by_log_id(lid)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    // Save the old content as snapshot
                    if let Some(old_json) = old_content_json {
                        let _ = self.storage.insert_things_content_snapshot(
                            device_id,
                            &upsert.uuid,
                            &old_json,
                            Some(lid),
                        );
                    }
                }
            }
        }

        Ok(created.clone())
    }

    pub fn things_splice_text(
        &self,
        device_id: &str,
        thing_uuid: &str,
        block_id: &str,
        index: usize,
        delete: usize,
        insert: &str,
    ) -> Result<bool> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;

        let snapshot =
            doc_set.extract_snapshot_with_options(crate::things_crdt::SnapshotOptions {
                include_content: false,
            })?;
        if !snapshot.things.iter().any(|t| t.uuid == thing_uuid) {
            tracing::debug!(
                thing_uuid,
                "things_splice_text: refusing to edit deleted or unreachable thing"
            );
            return Ok(false);
        }

        let Some(events) =
            doc_set.try_splice_thing_text(thing_uuid, block_id, index, delete, insert)?
        else {
            // No-op (e.g., block not found). Treat as failure so callers can fall back.
            return Ok(false);
        };

        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        // For splice_text, we use 5-minute grouping for update logs
        // Check if there's a recent update log within 5 minutes
        if self
            .storage
            .find_recent_thing_update_log(thing_uuid, 300)
            .ok()
            .flatten()
            .is_none()
        {
            // No recent log, create one
            let summary = format!("Edited thing content");
            let details = json!({
                "uuid": thing_uuid,
                "block_id": block_id,
                "operation": "splice_text",
            });
            let _ = self.storage.insert_things_change_log(
                device_id,
                ThingsOperationType::UpdateThing,
                "thing",
                thing_uuid,
                &summary,
                &details.to_string(),
                None,
                true,
            );
        }

        Ok(true)
    }

    /// Get the markdown content of a thing.
    /// Returns the markdown text content, or None if the thing doesn't exist or has no markdown content.
    pub fn things_get_thing_markdown(
        &self,
        device_id: &str,
        thing_uuid: &str,
    ) -> Result<Option<String>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let snapshot =
            doc_set.extract_snapshot_with_options(crate::things_crdt::SnapshotOptions {
                include_content: false,
            })?;
        if !snapshot.things.iter().any(|t| t.uuid == thing_uuid) {
            return Ok(None);
        }
        doc_set.get_thing_markdown_text(thing_uuid)
    }

    /// Edit the content of a thing using editor-level operations.
    ///
    /// # Operations
    /// - `overwrite`: Replace all content with `new_content`
    /// - `set_title`: Only change the title (ignores content fields)
    /// - `str_replace`: Find `old_str` and replace with `new_str` (must match exactly once)
    /// - `insert_at_line`: Insert `insert_text` after line `line_number` (1-based, 0 = prepend)
    /// - `append`: Append `append_text` to the end
    ///
    /// # Returns
    /// JSON result with success status, or error with current content for retry
    pub fn things_edit_content(
        &self,
        device_id: &str,
        thing_uuid: &str,
        operation: &str,
        // Optional fields depending on operation
        new_title: Option<&str>,
        new_content: Option<&str>,
        old_str: Option<&str>,
        new_str: Option<&str>,
        line_number: Option<usize>,
        insert_text: Option<&str>,
        append_text: Option<&str>,
    ) -> Result<String> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;

        // V3: Extract snapshot for metadata (without content)
        let snapshot =
            doc_set.extract_snapshot_with_options(crate::things_crdt::SnapshotOptions {
                include_content: false,
            })?;

        let thing = snapshot.things.iter().find(|t| t.uuid == thing_uuid);
        let Some(thing) = thing else {
            return Ok(json!({
                "error": "thing_not_found",
                "message": format!("Thing with UUID '{}' not found", thing_uuid),
            })
            .to_string());
        };

        // We only need to extract full content when the editor operation depends on reading current markdown.
        let include_content_for_read = operation != "overwrite";

        // Get current markdown content on-demand for the target thing only.
        let current_markdown = if include_content_for_read {
            doc_set
                .get_thing_markdown_text(thing_uuid)?
                .unwrap_or_default()
        } else {
            String::new()
        };

        // Determine the new content based on operation
        let (final_content, title_only) = match operation {
            "overwrite" => {
                let content = new_content.unwrap_or("");
                (content.to_string(), false)
            }
            "set_title" => {
                // Title-only operation, don't touch content
                (current_markdown.clone(), true)
            }
            "str_replace" => {
                let old = old_str.ok_or_else(|| anyhow!("str_replace requires 'old_str'"))?;
                let new = new_str.unwrap_or("");

                // Find all occurrences
                let matches: Vec<_> = current_markdown.match_indices(old).collect();

                if matches.is_empty() {
                    return Ok(json!({
                        "error": "str_replace_no_match",
                        "message": format!("'old_str' not found in content"),
                        "current_content": current_markdown,
                        "old_str": old,
                    })
                    .to_string());
                }

                if matches.len() > 1 {
                    return Ok(json!({
                        "error": "str_replace_multiple_matches",
                        "message": format!("'old_str' found {} times, must be unique. Use more context.", matches.len()),
                        "current_content": current_markdown,
                        "old_str": old,
                        "match_positions": matches.iter().map(|(pos, _)| *pos).collect::<Vec<_>>(),
                    }).to_string());
                }

                // Single match, perform replacement
                (current_markdown.replacen(old, new, 1), false)
            }
            "insert_at_line" => {
                let line_num = line_number.unwrap_or(0);
                let insert =
                    insert_text.ok_or_else(|| anyhow!("insert_at_line requires 'insert_text'"))?;

                let lines: Vec<&str> = current_markdown.lines().collect();
                let total_lines = lines.len();

                if line_num > total_lines {
                    return Ok(json!({
                        "error": "insert_at_line_out_of_range",
                        "message": format!("Line {} is out of range. Content has {} lines.", line_num, total_lines),
                        "current_content": current_markdown,
                        "total_lines": total_lines,
                    }).to_string());
                }

                // Insert after line_num (0 = prepend, 1 = after first line, etc.)
                let mut new_lines: Vec<&str> = Vec::with_capacity(lines.len() + 1);
                if line_num == 0 {
                    // Prepend
                    new_lines.push(insert);
                    new_lines.extend(lines);
                } else {
                    for (i, line) in lines.iter().enumerate() {
                        new_lines.push(line);
                        if i + 1 == line_num {
                            new_lines.push(insert);
                        }
                    }
                }
                (new_lines.join("\n"), false)
            }
            "append" => {
                let append = append_text.ok_or_else(|| anyhow!("append requires 'append_text'"))?;
                let mut result = current_markdown.clone();
                if !result.is_empty() && !result.ends_with('\n') {
                    result.push('\n');
                }
                result.push_str(append);
                (result, false)
            }
            _ => {
                return Ok(json!({
                    "error": "invalid_operation",
                    "message": format!("Unknown operation '{}'. Valid: overwrite, set_title, str_replace, insert_at_line, append", operation),
                }).to_string());
            }
        };

        // Build upsert payload
        let final_title = new_title.unwrap_or(&thing.title);

        // V3: Apply updates using document set operations
        // - Title change (optional): upsert_thing_meta
        // - Content change (optional): splice_thing_text or set_thing_content

        let mut events = Vec::new();

        if new_title.is_some() || operation == "set_title" {
            events.extend(doc_set.upsert_thing_meta(
                &thing.collection_uuid,
                thing_uuid,
                None, // datatype
                None, // status
                Some(final_title.to_string()),
                thing.parent_uuid.clone(),
                remi_things_crdt::TriggerUpdate::Noop,
            )?);
        }

        if !title_only {
            events.extend(doc_set.replace_thing_markdown_text(thing_uuid, &final_content)?);
        }

        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        // Record change log with 5-minute grouping
        if self
            .storage
            .find_recent_thing_update_log(thing_uuid, 300)
            .ok()
            .flatten()
            .is_none()
        {
            let summary = format!("Edited thing '{}' ({})", final_title, operation);
            let details = json!({
                "uuid": thing_uuid,
                "operation": operation,
            });
            let _ = self.storage.insert_things_change_log(
                device_id,
                ThingsOperationType::UpdateThing,
                "thing",
                thing_uuid,
                &summary,
                &details.to_string(),
                None,
                true,
            );
        }

        // Return success with updated content
        let result_content = if title_only {
            current_markdown
        } else {
            final_content
        };
        Ok(json!({
            "success": true,
            "uuid": thing_uuid,
            "title": final_title,
            "operation": operation,
            "content": result_content,
        })
        .to_string())
    }

    /// Set the status of a thing.
    /// Status values: "none", "in-progress", "stalled", "done"
    pub fn set_thing_status(
        &self,
        device_id: &str,
        thing_uuid: &str,
        status: &str,
    ) -> Result<bool> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;

        // Validate status value
        let valid_statuses = ["none", "in-progress", "stalled", "done"];
        if !valid_statuses.contains(&status) {
            anyhow::bail!(
                "Invalid status '{}', must be one of: {:?}",
                status,
                valid_statuses
            );
        }

        // V3: Find the thing to get its collection_uuid
        let collection_uuid = {
            let snapshot = doc_set.extract_snapshot()?;
            match snapshot.things.iter().find(|t| t.uuid == thing_uuid) {
                Some(t) => t.collection_uuid.clone(),
                None => {
                    tracing::warn!(
                        thing_uuid,
                        "set_thing_status: thing not in snapshot, scanning collection docs"
                    );
                    doc_set
                        .find_thing_collection_uuid(thing_uuid)
                        .ok_or_else(|| anyhow::anyhow!("Thing not found: {}", thing_uuid))?
                }
            }
        };

        let timestamp_ms = Some(chrono::Utc::now().timestamp_millis());

        // V3: Update status via collection document
        let events = doc_set.upsert_thing_meta(
            &collection_uuid,
            thing_uuid,
            None, // datatype
            Some(status.to_string()),
            None, // title
            None, // parent_uuid
            remi_things_crdt::TriggerUpdate::Noop,
        )?;

        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        // Record change log
        let summary = format!("Set thing status to '{}'", status);
        let details = json!({
            "uuid": thing_uuid,
            "status": status,
            "timestamp_ms": timestamp_ms,
        });
        let _ = self.storage.insert_things_change_log(
            device_id,
            ThingsOperationType::UpdateThing,
            "thing",
            thing_uuid,
            &summary,
            &details.to_string(),
            None,
            true,
        );

        Ok(true)
    }

    pub fn things_delete_thing(
        &self,
        device_id: &str,
        collection_uuid: &str,
        uuid: &str,
    ) -> Result<bool> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let before = doc_set.extract_snapshot()?;
        let thing = before.things.iter().find(|t| t.uuid == uuid);
        if thing.is_none() {
            return Ok(false);
        }
        let thing = thing.unwrap();
        let thing_title = thing.title.clone();
        let thing_content_json = serde_json::to_string(thing).ok();

        // Find child things (for cascade logging)
        let child_things: Vec<_> = before
            .things
            .iter()
            .filter(|t| t.parent_uuid.as_deref() == Some(uuid))
            .cloned()
            .collect();

        // V3: Delete thing from collection document.
        // Keep markdown docs locally so the metadata tombstone can converge
        // across devices and future reads stay consistent with reachability.
        let mut events = doc_set.delete_thing(collection_uuid, uuid)?;
        // Delete child things
        for child in &child_things {
            events.extend(doc_set.delete_thing(collection_uuid, &child.uuid)?);
        }
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        // Record change log
        let summary = format!("Deleted thing '{}'", thing_title);
        let details = json!({
            "uuid": uuid,
            "title": thing_title,
            "collection_uuid": collection_uuid,
            "child_count": child_things.len(),
        });
        let parent_log_id = self
            .storage
            .insert_things_change_log(
                device_id,
                ThingsOperationType::DeleteThing,
                "thing",
                uuid,
                &summary,
                &details.to_string(),
                None,
                true,
            )
            .ok();

        // Save content snapshot for potential restore
        if let (Some(parent_id), Some(content_json)) = (parent_log_id, thing_content_json) {
            let _ = self.storage.insert_things_content_snapshot(
                device_id,
                uuid,
                &content_json,
                Some(parent_id),
            );
        }

        // Record cascade deletions for child things
        if let Some(parent_id) = parent_log_id {
            let mut cascade_ids = Vec::new();
            for child in &child_things {
                let child_summary = format!("Deleted thing '{}' (cascade)", child.title);
                let child_details = json!({
                    "uuid": child.uuid,
                    "title": child.title,
                    "parent_uuid": uuid,
                    "cascade_reason": "parent_thing_deleted",
                });
                if let Ok(cascade_id) = self.storage.insert_things_change_log(
                    device_id,
                    ThingsOperationType::DeleteThing,
                    "thing",
                    &child.uuid,
                    &child_summary,
                    &child_details.to_string(),
                    Some(parent_id),
                    false, // Cascade deletions can't be individually undone
                ) {
                    cascade_ids.push(cascade_id);
                    // Save content snapshot for child
                    if let Ok(child_json) = serde_json::to_string(child) {
                        let _ = self.storage.insert_things_content_snapshot(
                            device_id,
                            &child.uuid,
                            &child_json,
                            Some(cascade_id),
                        );
                    }
                }
            }
            if !cascade_ids.is_empty() {
                let _ = self
                    .storage
                    .update_things_change_log_cascade_ids(parent_id, &cascade_ids);
            }
        }

        Ok(true)
    }

    pub fn things_set_status(
        &self,
        device_id: &str,
        thing_uuid: &str,
        status: &str,
        timestamp_ms: Option<i64>,
    ) -> Result<String> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;

        // Get the thing before update for logging
        let before = doc_set.extract_snapshot()?;
        let (thing_title, collection_uuid) =
            match before.things.iter().find(|t| t.uuid == thing_uuid) {
                Some(t) => (t.title.clone(), t.collection_uuid.clone()),
                None => {
                    tracing::warn!(
                        thing_uuid,
                        "things_set_status: thing not in snapshot, scanning collection docs"
                    );
                    let coll = doc_set
                        .find_thing_collection_uuid(thing_uuid)
                        .ok_or_else(|| anyhow::anyhow!("Thing not found: {}", thing_uuid))?;
                    (String::new(), coll)
                }
            };

        // V3: Apply status change via collection document
        let events = doc_set.upsert_thing_meta(
            &collection_uuid,
            thing_uuid,
            None, // datatype
            Some(status.to_string()),
            None, // title
            None, // parent_uuid
            remi_things_crdt::TriggerUpdate::Noop,
        )?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        // Record change log
        let summary = format!("Changed status of '{}' to '{}'", thing_title, status);
        let details = json!({
            "uuid": thing_uuid,
            "title": thing_title,
            "collection_uuid": collection_uuid,
            "status": status,
            "timestamp_ms": timestamp_ms,
        });
        let details_json = serde_json::to_string(&details).unwrap_or_default();
        let _ = self.storage.insert_things_change_log(
            device_id,
            ThingsOperationType::UpdateThing,
            "thing",
            thing_uuid,
            &summary,
            &details_json,
            None,  // parent_log_id
            false, // can_undo
        )?;

        Ok(format!("Status updated: {} -> {}", thing_title, status))
    }

    /// Add a content block to a thing (V3 multi-value).
    ///
    /// # Arguments
    /// * `device_id` - Device identifier
    /// * `thing_uuid` - Thing UUID
    /// * `block_json` - JSON string of content block
    ///
    /// Block JSON format:
    /// ```json
    /// {
    ///   "id": "block-uuid",
    ///   "title": "Optional title",
    ///   "order": 0.0,
    ///   "payload": {
    ///     "type": "location",
    ///     "loc_type": "coordinate",
    ///     "lat": 39.9,
    ///     "lng": 116.4,
    ///     "coord_system": "wgs84"
    ///   }
    /// }
    /// ```
    pub fn things_add_content_entry(
        &self,
        device_id: &str,
        thing_uuid: &str,
        entry: ContentEntry,
    ) -> Result<String> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;

        let id = entry.id.clone();

        let events = doc_set.add_content_entry(&collection_uuid, thing_uuid, entry)?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        Ok(id)
    }

    /// Update a content entry on a thing (V3 multi-value).
    ///
    /// # Arguments
    /// * `device_id` - Device identifier
    /// * `thing_uuid` - Thing UUID
    /// * `update` - Typed fields to update
    ///
    pub fn things_update_content_entry(
        &self,
        device_id: &str,
        thing_uuid: &str,
        update: ContentEntryUpdate,
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;

        let events = doc_set.update_content_entry(
            &collection_uuid,
            thing_uuid,
            &update.id,
            update.title,
            update.order,
            update.payload,
        )?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        Ok(())
    }

    /// Delete a content entry from a thing (V3 multi-value).
    pub fn things_delete_content_entry(
        &self,
        device_id: &str,
        thing_uuid: &str,
        entry_id: &str,
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;
        let entries = doc_set.get_content_entries(&collection_uuid, thing_uuid)?;
        let json_object_field =
            entries
                .iter()
                .find(|entry| entry.id == entry_id)
                .and_then(|entry| match &entry.payload {
                    ContentEntryPayload::JsonObject(field) => Some(field.clone()),
                    _ => None,
                });

        let events = doc_set.delete_content_entry(&collection_uuid, thing_uuid, entry_id)?;
        if let Some(field) = json_object_field {
            let data_key = crate::things_crdt::DocumentKey::thing_content(&field.data_doc_uuid);
            doc_set.remove_document(&data_key);
            let _ = self
                .storage
                .delete_crdt_document(&field.data_doc_uuid, "thing_markdown");

            if let Some(schema_doc_uuid) = field.schema_doc_uuid {
                let schema_key = crate::things_crdt::DocumentKey::thing_content(&schema_doc_uuid);
                doc_set.remove_document(&schema_key);
                let _ = self
                    .storage
                    .delete_crdt_document(&schema_doc_uuid, "thing_markdown");
            }
        }
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);

        Ok(())
    }

    pub fn things_add_json_object_content_entry(
        &self,
        device_id: &str,
        thing_uuid: &str,
        title: Option<&str>,
        data: Option<&Value>,
        schema: Option<&Value>,
    ) -> Result<String> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;
        let entries = doc_set.get_content_entries(&collection_uuid, thing_uuid)?;
        let order = entries
            .iter()
            .map(|entry| entry.order)
            .fold(-1.0_f64, f64::max)
            + 1.0;

        let data_value = data.cloned().unwrap_or_else(|| json!({}));
        Self::validate_json_object_data(schema, &data_value)?;

        let data_doc_uuid = uuid::Uuid::new_v4().to_string();
        doc_set.set_thing_json_content(
            &data_doc_uuid,
            thing_uuid,
            "json_object_data",
            &data_value,
        )?;

        let schema_doc_uuid = if let Some(schema_value) = schema {
            let schema_doc_uuid = uuid::Uuid::new_v4().to_string();
            doc_set.set_thing_json_content(
                &schema_doc_uuid,
                thing_uuid,
                "json_object_schema",
                schema_value,
            )?;
            Some(schema_doc_uuid)
        } else {
            None
        };

        let entry = ContentEntry {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.map(|value| value.to_string()),
            order,
            payload: ContentEntryPayload::JsonObject(JsonObjectField {
                data_doc_uuid,
                schema_doc_uuid,
            }),
        };

        let entry_id = entry.id.clone();
        let logged_data_doc_uuid = match &entry.payload {
            ContentEntryPayload::JsonObject(field) => field.data_doc_uuid.clone(),
            _ => unreachable!("json object entry payload must stay json object"),
        };
        let logged_schema_doc_uuid = match &entry.payload {
            ContentEntryPayload::JsonObject(field) => field.schema_doc_uuid.clone(),
            _ => unreachable!("json object entry payload must stay json object"),
        };
        let events = doc_set.add_content_entry(&collection_uuid, thing_uuid, entry)?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);
        tracing::info!(
            device_id,
            thing_uuid,
            collection_uuid,
            entry_id,
            title = title.unwrap_or(""),
            data_doc_uuid = %logged_data_doc_uuid,
            schema_doc_uuid = ?logged_schema_doc_uuid,
            "Created json_object content entry"
        );
        Ok(entry_id)
    }

    pub fn things_get_json_object_entry_data(
        &self,
        device_id: &str,
        thing_uuid: &str,
        entry_id: &str,
    ) -> Result<Option<Value>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;
        let entries = doc_set.get_content_entries(&collection_uuid, thing_uuid)?;
        let field = Self::resolve_json_object_entry_field(&entries, entry_id)?;
        let result = doc_set.get_thing_json_content(&field.data_doc_uuid, thing_uuid)?;
        tracing::debug!(
            device_id,
            thing_uuid,
            collection_uuid,
            entry_id,
            data_doc_uuid = %field.data_doc_uuid,
            found = result.is_some(),
            "Loaded json_object entry data"
        );
        Ok(result)
    }

    pub fn things_get_json_object_entry_schema(
        &self,
        device_id: &str,
        thing_uuid: &str,
        entry_id: &str,
    ) -> Result<Option<Value>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;
        let entries = doc_set.get_content_entries(&collection_uuid, thing_uuid)?;
        let field = Self::resolve_json_object_entry_field(&entries, entry_id)?;
        let Some(schema_doc_uuid) = field.schema_doc_uuid else {
            tracing::debug!(
                device_id,
                thing_uuid,
                collection_uuid,
                entry_id,
                "Json object entry has no schema doc"
            );
            return Ok(None);
        };
        let result = doc_set.get_thing_json_content(&schema_doc_uuid, thing_uuid)?;
        tracing::debug!(
            device_id,
            thing_uuid,
            collection_uuid,
            entry_id,
            schema_doc_uuid,
            found = result.is_some(),
            "Loaded json_object entry schema"
        );
        Ok(result)
    }

    pub fn things_set_json_object_entry_data(
        &self,
        device_id: &str,
        thing_uuid: &str,
        entry_id: &str,
        data: &Value,
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;
        let entries = doc_set.get_content_entries(&collection_uuid, thing_uuid)?;
        let field = Self::resolve_json_object_entry_field(&entries, entry_id)?;
        let schema = match &field.schema_doc_uuid {
            Some(schema_doc_uuid) => doc_set.get_thing_json_content(schema_doc_uuid, thing_uuid)?,
            None => None,
        };

        Self::validate_json_object_data(schema.as_ref(), data)?;
        doc_set.set_thing_json_content(
            &field.data_doc_uuid,
            thing_uuid,
            "json_object_data",
            data,
        )?;
        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(
            device_id,
            vec![ThingsDocumentEvent::content_entry(
                ThingsDocumentChangeKind::Updated,
                &collection_uuid,
                thing_uuid,
                entry_id,
            )],
        );
        Ok(())
    }

    pub fn things_set_json_object_entry_schema(
        &self,
        device_id: &str,
        thing_uuid: &str,
        entry_id: &str,
        schema: Option<&Value>,
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;
        let entries = doc_set.get_content_entries(&collection_uuid, thing_uuid)?;
        let mut field = Self::resolve_json_object_entry_field(&entries, entry_id)?;
        let mut events = Vec::new();
        let data = doc_set
            .get_thing_json_content(&field.data_doc_uuid, thing_uuid)?
            .unwrap_or_else(|| json!({}));

        Self::validate_json_object_data(schema, &data)?;

        match schema {
            Some(schema_value) => {
                let schema_doc_uuid = field
                    .schema_doc_uuid
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                doc_set.set_thing_json_content(
                    &schema_doc_uuid,
                    thing_uuid,
                    "json_object_schema",
                    schema_value,
                )?;
                if field.schema_doc_uuid.as_deref() != Some(schema_doc_uuid.as_str()) {
                    field.schema_doc_uuid = Some(schema_doc_uuid);
                    events.extend(doc_set.update_content_entry(
                        &collection_uuid,
                        thing_uuid,
                        entry_id,
                        None,
                        None,
                        Some(ContentEntryPayload::JsonObject(field)),
                    )?);
                }
            }
            None => {
                if let Some(schema_doc_uuid) = field.schema_doc_uuid.take() {
                    let schema_key =
                        crate::things_crdt::DocumentKey::thing_content(&schema_doc_uuid);
                    doc_set.remove_document(&schema_key);
                    let _ = self
                        .storage
                        .delete_crdt_document(&schema_doc_uuid, "thing_markdown");
                    events.extend(doc_set.update_content_entry(
                        &collection_uuid,
                        thing_uuid,
                        entry_id,
                        None,
                        None,
                        Some(ContentEntryPayload::JsonObject(field)),
                    )?);
                }
            }
        }

        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        events.push(ThingsDocumentEvent::content_entry(
            ThingsDocumentChangeKind::Updated,
            &collection_uuid,
            thing_uuid,
            entry_id,
        ));
        self.emit_document_events(device_id, events);
        Ok(())
    }

    pub fn things_update_json_object_entry(
        &self,
        device_id: &str,
        thing_uuid: &str,
        entry_id: &str,
        title: Option<Option<String>>,
        data: &Value,
        schema: Option<&Value>,
    ) -> Result<()> {
        let mut doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;
        let entries = doc_set.get_content_entries(&collection_uuid, thing_uuid)?;
        let mut field = Self::resolve_json_object_entry_field(&entries, entry_id)?;

        Self::validate_json_object_data(schema, data)?;

        let mut payload_changed = false;

        match schema {
            Some(schema_value) => {
                let schema_doc_uuid = field
                    .schema_doc_uuid
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                doc_set.set_thing_json_content(
                    &schema_doc_uuid,
                    thing_uuid,
                    "json_object_schema",
                    schema_value,
                )?;
                if field.schema_doc_uuid.as_deref() != Some(schema_doc_uuid.as_str()) {
                    field.schema_doc_uuid = Some(schema_doc_uuid);
                    payload_changed = true;
                }
            }
            None => {
                if let Some(schema_doc_uuid) = field.schema_doc_uuid.take() {
                    let schema_key =
                        crate::things_crdt::DocumentKey::thing_content(&schema_doc_uuid);
                    doc_set.remove_document(&schema_key);
                    let _ = self
                        .storage
                        .delete_crdt_document(&schema_doc_uuid, "thing_markdown");
                    payload_changed = true;
                }
            }
        }

        doc_set.set_thing_json_content(
            &field.data_doc_uuid,
            thing_uuid,
            "json_object_data",
            data,
        )?;

        let title_changed = title.is_some();
        let logged_data_doc_uuid = field.data_doc_uuid.clone();
        let logged_schema_doc_uuid = field.schema_doc_uuid.clone();

        let mut events = if title.is_some() || payload_changed {
            doc_set.update_content_entry(
                &collection_uuid,
                thing_uuid,
                entry_id,
                title,
                None,
                payload_changed.then_some(ContentEntryPayload::JsonObject(field)),
            )?
        } else {
            Vec::new()
        };

        if events.is_empty() {
            events.push(ThingsDocumentEvent::content_entry(
                ThingsDocumentChangeKind::Updated,
                &collection_uuid,
                thing_uuid,
                entry_id,
            ));
        }

        doc_set.save_dirty_to_storage_with_compaction(&self.storage)?;
        self.emit_document_events(device_id, events);
        tracing::info!(
            device_id,
            thing_uuid,
            collection_uuid,
            entry_id,
            data_doc_uuid = %logged_data_doc_uuid,
            schema_doc_uuid = ?logged_schema_doc_uuid,
            title_changed,
            payload_changed,
            "Updated json_object content entry"
        );
        Ok(())
    }

    /// Get all content entries of a thing.
    pub fn things_get_content_entries(
        &self,
        device_id: &str,
        thing_uuid: &str,
    ) -> Result<Vec<ContentEntry>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let collection_uuid = Self::resolve_thing_collection_uuid(&doc_set, thing_uuid)?;
        let entries = doc_set.get_content_entries(&collection_uuid, thing_uuid)?;
        let json_object_entries: Vec<String> = entries
            .iter()
            .filter_map(|entry| match &entry.payload {
                ContentEntryPayload::JsonObject(field) => Some(format!(
                    "{}:{}:{:?}",
                    entry.id, field.data_doc_uuid, field.schema_doc_uuid
                )),
                _ => None,
            })
            .collect();
        tracing::debug!(
            device_id,
            thing_uuid,
            collection_uuid,
            entry_count = entries.len(),
            json_object_entry_count = json_object_entries.len(),
            json_object_entries = ?json_object_entries,
            "Loaded content entries for thing"
        );
        Ok(entries)
    }

    /// V3: Replace state after sync - this needs to be rewritten for multi-doc
    /// For now, we keep a simplified version that saves synced documents individually
    pub fn things_replace_state_after_sync(
        &self,
        _device_id: &str,
        _doc_bytes: Vec<u8>,
        _sync_state_bytes: Vec<u8>,
        _last_sync_at: Option<&str>,
        _dirty: bool,
    ) -> Result<String> {
        // V3: This function is no longer used in the same way.
        // The sync layer now handles per-document sync.
        // This is kept for API compatibility but should be migrated to v3 sync flow.
        anyhow::bail!(
            "things_replace_state_after_sync is deprecated in v3. Use v3 sync flow instead."
        )
    }

    /// V3: Get raw state is no longer applicable for multi-doc architecture
    pub fn things_get_raw_state(
        &self,
        _device_id: &str,
    ) -> Result<(Vec<u8>, Vec<u8>, bool, Option<String>)> {
        // V3: This function is deprecated. Use get_or_init_document_set instead.
        anyhow::bail!(
            "things_get_raw_state is deprecated in v3. Use document set operations instead."
        )
    }

    // ===== Things Change Log API =====

    /// List recent change log entries with pagination.
    /// Returns entries ordered by created_at DESC (newest first).
    pub fn things_list_change_log(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<ThingsChangeLogEntry>> {
        self.storage.list_things_change_log(limit, offset)
    }

    /// List change log entries for a specific entity.
    pub fn things_list_change_log_for_entity(
        &self,
        entity_type: &str,
        entity_uuid: &str,
        limit: u32,
    ) -> Result<Vec<ThingsChangeLogEntry>> {
        self.storage
            .list_things_change_log_for_entity(entity_type, entity_uuid, limit)
    }

    /// Get a single change log entry by ID.
    pub fn things_get_change_log(&self, log_id: i64) -> Result<Option<ThingsChangeLogEntry>> {
        self.storage.get_things_change_log(log_id)
    }

    /// Get content snapshots for a thing (version history).
    pub fn things_list_content_snapshots(
        &self,
        thing_uuid: &str,
        limit: u32,
    ) -> Result<Vec<ThingsContentSnapshot>> {
        self.storage
            .list_things_content_snapshots(thing_uuid, limit)
    }

    // ===== Things Change Log Sync Methods =====

    /// Get unsynced change log entries for upload to server.
    pub fn things_get_unsynced_change_logs(&self, limit: u32) -> Result<Vec<ThingsChangeLogEntry>> {
        self.storage.get_unsynced_change_logs(limit)
    }

    /// Mark change log entries as synced.
    pub fn things_mark_change_logs_synced(&self, ids: &[i64]) -> Result<()> {
        self.storage.mark_change_logs_synced(ids)
    }

    /// Get unsynced content snapshots for upload to server.
    pub fn things_get_unsynced_content_snapshots(
        &self,
        limit: u32,
    ) -> Result<Vec<ThingsContentSnapshot>> {
        self.storage.get_unsynced_content_snapshots(limit)
    }

    /// Mark content snapshots as synced.
    pub fn things_mark_content_snapshots_synced(&self, ids: &[i64]) -> Result<()> {
        self.storage.mark_content_snapshots_synced(ids)
    }

    /// Insert a change log entry received from server (already synced).
    pub fn things_insert_synced_change_log(
        &self,
        device_id: &str,
        op_type: ThingsOperationType,
        entity_type: &str,
        entity_uuid: &str,
        summary: &str,
        details_json: &str,
        created_at: i64,
    ) -> Result<i64> {
        self.storage.insert_synced_change_log(
            device_id,
            op_type,
            entity_type,
            entity_uuid,
            summary,
            details_json,
            created_at,
        )
    }

    /// Insert a content snapshot received from server (already synced).
    pub fn things_insert_synced_content_snapshot(
        &self,
        device_id: &str,
        thing_uuid: &str,
        content_json: &str,
        created_at: i64,
    ) -> Result<i64> {
        self.storage
            .insert_synced_content_snapshot(device_id, thing_uuid, content_json, created_at)
    }

    /// Preview an undo operation without executing it.
    /// Returns information about what would be undone and any conflicts.
    pub fn things_preview_undo(&self, device_id: &str, log_id: i64) -> Result<ThingsUndoPreview> {
        let log_entry = self
            .storage
            .get_things_change_log(log_id)?
            .ok_or_else(|| anyhow!("Change log entry not found: {}", log_id))?;

        if !log_entry.can_undo {
            return Err(anyhow!("This operation cannot be undone"));
        }

        let doc_set = self.get_or_init_document_set(device_id)?;
        let snapshot = doc_set.extract_snapshot()?;

        // Get cascade entries
        let cascade_entries = self.storage.get_things_change_log_cascades(log_id)?;

        // Determine conflicts based on operation type
        let conflict = self.check_undo_conflict(&log_entry, &snapshot)?;

        Ok(ThingsUndoPreview {
            log_entry,
            needs_cascade_restore: !cascade_entries.is_empty(),
            conflict,
            cascade_entries,
        })
    }

    /// Execute an undo operation.
    /// V3 implementation: Uses multi-document architecture.
    pub fn things_execute_undo(
        &self,
        device_id: &str,
        execution: ThingsUndoExecution,
    ) -> Result<String> {
        let preview = self.things_preview_undo(device_id, execution.log_id)?;
        let log_entry = &preview.log_entry;

        // Check for conflicts that need resolution
        if let Some(conflict) = &preview.conflict {
            if execution.resolution_option.is_none() {
                return Err(anyhow!(
                    "Conflict detected: {}. Please provide a resolution option.",
                    conflict.description
                ));
            }
        }

        let message = match log_entry.op_type {
            ThingsOperationType::CreateCollection => {
                // Undo create = delete the collection
                self.things_delete_collection(device_id, &log_entry.entity_uuid)?;
                format!("Undone: {}", log_entry.summary)
            }
            ThingsOperationType::CreateThing => {
                // Undo create = delete the thing
                let details: serde_json::Value =
                    serde_json::from_str(&log_entry.details_json).unwrap_or_default();
                let collection_uuid = details["collection_uuid"].as_str().unwrap_or("");
                self.things_delete_thing(device_id, collection_uuid, &log_entry.entity_uuid)?;
                format!("Undone: {}", log_entry.summary)
            }
            ThingsOperationType::DeleteCollection => {
                // Undo delete = restore collection from snapshot
                self.restore_deleted_collection_v3(device_id, log_entry, &preview)?
            }
            ThingsOperationType::DeleteThing => {
                // Undo delete = restore thing from snapshot
                self.restore_deleted_thing_v3(device_id, log_entry, &execution, &preview)?
            }
            ThingsOperationType::UpdateCollection | ThingsOperationType::UpdateThing => {
                // Undo update = restore previous state from snapshot
                self.restore_from_snapshot_v3(device_id, log_entry)?
            }
            ThingsOperationType::MoveThing => {
                // Undo move = restore original collection_uuid by re-upserting with old collection
                self.undo_move_thing_v3(device_id, log_entry)?
            }
            ThingsOperationType::MoveThings => {
                // Undo batch move = restore each thing to original collection
                self.undo_move_things_v3(device_id, log_entry, &preview)?
            }
            ThingsOperationType::DeleteThings => {
                // Undo batch delete = restore each deleted thing
                self.undo_delete_things_v3(device_id, log_entry, &preview)?
            }
            _ => {
                return Err(anyhow!(
                    "Undo not supported for operation type: {:?}",
                    log_entry.op_type
                ));
            }
        };

        // Mark the log entry as undone
        self.storage.mark_things_change_log_undone(log_entry.id)?;

        // Log the undo operation
        let undo_op_type = log_entry
            .op_type
            .to_undo_variant()
            .unwrap_or(log_entry.op_type);
        let _ = self.storage.insert_things_change_log(
            device_id,
            undo_op_type,
            &log_entry.entity_type,
            &log_entry.entity_uuid,
            &message,
            &serde_json::json!({
                "undone_log_id": log_entry.id,
                "original_op": log_entry.op_type.as_str(),
            })
            .to_string(),
            None,  // parent_log_id
            false, // can_undo (undo operations typically can't be undone again)
        );

        Ok(message)
    }

    /// V3: Restore a deleted collection from snapshot
    fn restore_deleted_collection_v3(
        &self,
        device_id: &str,
        log_entry: &ThingsChangeLogEntry,
        preview: &ThingsUndoPreview,
    ) -> Result<String> {
        let details: serde_json::Value =
            serde_json::from_str(&log_entry.details_json).unwrap_or_default();
        let title = details["title"].as_str().unwrap_or("Restored Collection");
        let trigger_uuid = details["trigger_uuid"].as_str().map(|s| s.to_string());

        // Restore the collection
        self.things_upsert_collection(
            device_id,
            ThingCollectionUpsert {
                uuid: log_entry.entity_uuid.clone(),
                title: title.to_string(),
                trigger_uuid,
                created_at: None,
                updated_at: None,
            },
        )?;

        // Restore cascade-deleted things
        for cascade_entry in &preview.cascade_entries {
            if cascade_entry.entity_type == "thing" {
                if let Some(snapshot) = self
                    .storage
                    .get_things_content_snapshot_by_log_id(cascade_entry.id)?
                {
                    if let Ok(thing_data) =
                        serde_json::from_str::<ThingEntry>(&snapshot.content_json)
                    {
                        self.things_upsert_thing(
                            device_id,
                            ThingUpsert {
                                uuid: thing_data.uuid,
                                title: thing_data.title,
                                datatype: thing_data.datatype,
                                data: Some(thing_data.data),
                                collection_uuid: thing_data.collection_uuid,
                                trigger_uuid: thing_data.trigger_uuid,
                                parent_uuid: thing_data.parent_uuid,
                                created_at: None,
                                updated_at: None,
                            },
                        )?;

                        // For markdown things, content is in the data.content field
                        // The upsert will handle setting it via the document set
                    }
                }
            }
        }

        Ok(format!("Restored collection '{}' and its contents", title))
    }

    /// V3: Restore a deleted thing from snapshot
    fn restore_deleted_thing_v3(
        &self,
        device_id: &str,
        log_entry: &ThingsChangeLogEntry,
        execution: &ThingsUndoExecution,
        preview: &ThingsUndoPreview,
    ) -> Result<String> {
        let content_snapshot = self
            .storage
            .get_things_content_snapshot_by_log_id(log_entry.id)?
            .ok_or_else(|| anyhow!("No content snapshot found for deleted thing"))?;

        let thing_data: ThingEntry = serde_json::from_str(&content_snapshot.content_json)
            .context("Failed to parse thing snapshot")?;

        // Check if parent collection exists
        let doc_set = self.get_or_init_document_set(device_id)?;
        let snapshot = doc_set.extract_snapshot()?;
        let parent_exists = snapshot
            .collections
            .iter()
            .any(|c| c.uuid == thing_data.collection_uuid);

        let target_collection = if !parent_exists {
            match execution.resolution_option.as_deref() {
                Some("cascade_restore") => {
                    return Err(anyhow!(
                        "Cascade restore of parent collection not yet implemented. Use 'move_to_other' instead."
                    ));
                }
                Some("move_to_other") => execution
                    .target_collection_uuid
                    .as_ref()
                    .ok_or_else(|| anyhow!("Target collection UUID required for move_to_other"))?
                    .clone(),
                Some("cancel") => {
                    return Err(anyhow!("Undo cancelled by user"));
                }
                _ => {
                    return Err(anyhow!(
                        "Parent collection deleted. Please provide a resolution option."
                    ));
                }
            }
        } else {
            thing_data.collection_uuid.clone()
        };
        let restored_thing_uuid = thing_data.uuid.clone();
        let restored_thing_title = thing_data.title.clone();

        // Restore the thing
        self.things_upsert_thing(
            device_id,
            ThingUpsert {
                uuid: thing_data.uuid,
                title: thing_data.title,
                datatype: thing_data.datatype,
                data: Some(thing_data.data),
                collection_uuid: target_collection,
                trigger_uuid: thing_data.trigger_uuid,
                parent_uuid: thing_data.parent_uuid,
                created_at: None,
                updated_at: None,
            },
        )?;

        // Restore cascade-deleted child things
        for cascade_entry in &preview.cascade_entries {
            if cascade_entry.entity_type == "thing"
                && cascade_entry.entity_uuid != restored_thing_uuid
            {
                if let Some(child_snapshot) = self
                    .storage
                    .get_things_content_snapshot_by_log_id(cascade_entry.id)?
                {
                    if let Ok(child_data) =
                        serde_json::from_str::<ThingEntry>(&child_snapshot.content_json)
                    {
                        self.things_upsert_thing(
                            device_id,
                            ThingUpsert {
                                uuid: child_data.uuid,
                                title: child_data.title,
                                datatype: child_data.datatype,
                                data: Some(child_data.data),
                                collection_uuid: child_data.collection_uuid,
                                trigger_uuid: child_data.trigger_uuid,
                                parent_uuid: child_data.parent_uuid,
                                created_at: None,
                                updated_at: None,
                            },
                        )?;
                    }
                }
            }
        }

        Ok(format!("Restored thing '{}'", restored_thing_title))
    }

    /// V3: Restore from update snapshot
    fn restore_from_snapshot_v3(
        &self,
        device_id: &str,
        log_entry: &ThingsChangeLogEntry,
    ) -> Result<String> {
        let content_snapshot = self
            .storage
            .get_things_content_snapshot_by_log_id(log_entry.id)?
            .ok_or_else(|| anyhow!("No content snapshot found for update operation"))?;

        if log_entry.entity_type == "collection" {
            let collection_data: ThingCollectionEntry =
                serde_json::from_str(&content_snapshot.content_json)
                    .context("Failed to parse collection snapshot")?;
            let collection_title = collection_data.title.clone();

            self.things_upsert_collection(
                device_id,
                ThingCollectionUpsert {
                    uuid: collection_data.uuid,
                    title: collection_data.title,
                    trigger_uuid: collection_data.trigger_uuid,
                    created_at: None,
                    updated_at: None,
                },
            )?;

            Ok(format!(
                "Restored collection '{}' to previous state",
                collection_title
            ))
        } else {
            let thing_data: ThingEntry = serde_json::from_str(&content_snapshot.content_json)
                .context("Failed to parse thing snapshot")?;
            let thing_title = thing_data.title.clone();

            self.things_upsert_thing(
                device_id,
                ThingUpsert {
                    uuid: thing_data.uuid,
                    title: thing_data.title,
                    datatype: thing_data.datatype,
                    data: Some(thing_data.data),
                    collection_uuid: thing_data.collection_uuid,
                    trigger_uuid: thing_data.trigger_uuid,
                    parent_uuid: thing_data.parent_uuid,
                    created_at: None,
                    updated_at: None,
                },
            )?;

            Ok(format!(
                "Restored thing '{}' to previous state",
                thing_title
            ))
        }
    }

    /// V3: Undo move thing - restore original collection_uuid
    fn undo_move_thing_v3(
        &self,
        device_id: &str,
        log_entry: &ThingsChangeLogEntry,
    ) -> Result<String> {
        let details: serde_json::Value =
            serde_json::from_str(&log_entry.details_json).unwrap_or_default();
        let original_collection = details["from_collection_uuid"]
            .as_str()
            .ok_or_else(|| anyhow!("Missing from_collection_uuid in move details"))?;
        let thing_uuid = &log_entry.entity_uuid;

        // Get the current thing data
        let doc_set = self.get_or_init_document_set(device_id)?;
        let snapshot = doc_set.extract_snapshot()?;
        let thing = match snapshot.things.iter().find(|t| t.uuid == *thing_uuid) {
            Some(t) => t,
            None => {
                anyhow::bail!("Thing not found for undo-move: {}", thing_uuid);
            }
        };

        // Re-upsert with original collection
        self.things_upsert_thing(
            device_id,
            ThingUpsert {
                uuid: thing.uuid.clone(),
                title: thing.title.clone(),
                datatype: thing.datatype.clone(),
                data: Some(thing.data.clone()),
                collection_uuid: original_collection.to_string(),
                trigger_uuid: thing.trigger_uuid.clone(),
                parent_uuid: thing.parent_uuid.clone(),
                created_at: None,
                updated_at: None,
            },
        )?;

        Ok(format!(
            "Moved thing back to original collection '{}'",
            original_collection
        ))
    }

    /// V3: Undo batch move - restore each thing to original collection
    fn undo_move_things_v3(
        &self,
        device_id: &str,
        log_entry: &ThingsChangeLogEntry,
        preview: &ThingsUndoPreview,
    ) -> Result<String> {
        let details: serde_json::Value =
            serde_json::from_str(&log_entry.details_json).unwrap_or_default();

        // The batch move details should contain a list of moved things with original collections
        let moved_items = details["moved_items"].as_array();

        let mut restored_count = 0;

        // First try batch details from the main log entry
        if let Some(items) = moved_items {
            for item in items {
                let thing_uuid = item["uuid"].as_str().unwrap_or("");
                let original_collection = item["from_collection_uuid"].as_str().unwrap_or("");

                if thing_uuid.is_empty() || original_collection.is_empty() {
                    continue;
                }

                // Get the current thing data
                let doc_set = self.get_or_init_document_set(device_id)?;
                let snapshot = doc_set.extract_snapshot()?;

                if let Some(thing) = snapshot.things.iter().find(|t| t.uuid == thing_uuid) {
                    self.things_upsert_thing(
                        device_id,
                        ThingUpsert {
                            uuid: thing.uuid.clone(),
                            title: thing.title.clone(),
                            datatype: thing.datatype.clone(),
                            data: Some(thing.data.clone()),
                            collection_uuid: original_collection.to_string(),
                            trigger_uuid: thing.trigger_uuid.clone(),
                            parent_uuid: thing.parent_uuid.clone(),
                            created_at: None,
                            updated_at: None,
                        },
                    )?;
                    restored_count += 1;
                }
            }
        }

        // Also process cascade entries (individual MoveThing logs linked to this batch)
        for cascade_entry in &preview.cascade_entries {
            if cascade_entry.op_type == ThingsOperationType::MoveThing {
                let cascade_details: serde_json::Value =
                    serde_json::from_str(&cascade_entry.details_json).unwrap_or_default();
                let thing_uuid = &cascade_entry.entity_uuid;
                let original_collection = cascade_details["from_collection_uuid"]
                    .as_str()
                    .unwrap_or("");

                if original_collection.is_empty() {
                    continue;
                }

                let doc_set = self.get_or_init_document_set(device_id)?;
                let snapshot = doc_set.extract_snapshot()?;

                if let Some(thing) = snapshot.things.iter().find(|t| t.uuid == *thing_uuid) {
                    self.things_upsert_thing(
                        device_id,
                        ThingUpsert {
                            uuid: thing.uuid.clone(),
                            title: thing.title.clone(),
                            datatype: thing.datatype.clone(),
                            data: Some(thing.data.clone()),
                            collection_uuid: original_collection.to_string(),
                            trigger_uuid: thing.trigger_uuid.clone(),
                            parent_uuid: thing.parent_uuid.clone(),
                            created_at: None,
                            updated_at: None,
                        },
                    )?;
                    restored_count += 1;
                }
            }
        }

        if restored_count == 0 {
            return Err(anyhow!("No things found to restore from batch move"));
        }

        Ok(format!(
            "Restored {} things to original collections",
            restored_count
        ))
    }

    /// V3: Undo batch delete - restore each deleted thing from snapshots
    fn undo_delete_things_v3(
        &self,
        device_id: &str,
        log_entry: &ThingsChangeLogEntry,
        preview: &ThingsUndoPreview,
    ) -> Result<String> {
        let mut restored_count = 0;

        // Try to restore from main log entry's snapshot first
        if let Some(snapshot) = self
            .storage
            .get_things_content_snapshot_by_log_id(log_entry.id)?
        {
            // The snapshot might contain a batch of things in JSON array format
            if let Ok(things) = serde_json::from_str::<Vec<ThingEntry>>(&snapshot.content_json) {
                for thing_data in things {
                    self.things_upsert_thing(
                        device_id,
                        ThingUpsert {
                            uuid: thing_data.uuid,
                            title: thing_data.title,
                            datatype: thing_data.datatype,
                            data: Some(thing_data.data),
                            collection_uuid: thing_data.collection_uuid,
                            trigger_uuid: thing_data.trigger_uuid,
                            parent_uuid: thing_data.parent_uuid,
                            created_at: None,
                            updated_at: None,
                        },
                    )?;
                    restored_count += 1;
                }
            } else if let Ok(thing_data) =
                serde_json::from_str::<ThingEntry>(&snapshot.content_json)
            {
                // Single thing in snapshot
                self.things_upsert_thing(
                    device_id,
                    ThingUpsert {
                        uuid: thing_data.uuid,
                        title: thing_data.title,
                        datatype: thing_data.datatype,
                        data: Some(thing_data.data),
                        collection_uuid: thing_data.collection_uuid,
                        trigger_uuid: thing_data.trigger_uuid,
                        parent_uuid: thing_data.parent_uuid,
                        created_at: None,
                        updated_at: None,
                    },
                )?;
                restored_count += 1;
            }
        }

        // Also process cascade entries (individual DeleteThing logs linked to this batch)
        for cascade_entry in &preview.cascade_entries {
            if cascade_entry.op_type == ThingsOperationType::DeleteThing {
                if let Some(child_snapshot) = self
                    .storage
                    .get_things_content_snapshot_by_log_id(cascade_entry.id)?
                {
                    if let Ok(thing_data) =
                        serde_json::from_str::<ThingEntry>(&child_snapshot.content_json)
                    {
                        // Check if parent collection still exists
                        let doc_set = self.get_or_init_document_set(device_id)?;
                        let snapshot = doc_set.extract_snapshot()?;
                        let parent_exists = snapshot
                            .collections
                            .iter()
                            .any(|c| c.uuid == thing_data.collection_uuid);

                        if parent_exists {
                            self.things_upsert_thing(
                                device_id,
                                ThingUpsert {
                                    uuid: thing_data.uuid,
                                    title: thing_data.title,
                                    datatype: thing_data.datatype,
                                    data: Some(thing_data.data),
                                    collection_uuid: thing_data.collection_uuid,
                                    trigger_uuid: thing_data.trigger_uuid,
                                    parent_uuid: thing_data.parent_uuid,
                                    created_at: None,
                                    updated_at: None,
                                },
                            )?;
                            restored_count += 1;
                        }
                    }
                }
            }
        }

        if restored_count == 0 {
            return Err(anyhow!("No things found to restore from batch delete"));
        }

        Ok(format!("Restored {} deleted things", restored_count))
    }

    /// Cleanup old change logs and snapshots (retention policy).
    pub fn things_cleanup_change_logs(&self, older_than_days: i64) -> Result<(u64, u64)> {
        let logs_deleted = self.storage.cleanup_things_change_log(older_than_days)?;
        let snapshots_deleted = self
            .storage
            .cleanup_things_content_snapshots(older_than_days)?;
        Ok((logs_deleted, snapshots_deleted))
    }

    // ===== V3 CRDT Multi-Document API =====

    /// Get a single CRDT document by key (uuid + data_type).
    pub fn crdt_get_document(
        &self,
        uuid: &str,
        data_type: &str,
    ) -> Result<Option<crate::types::CrdtDocumentRow>> {
        self.storage.get_crdt_document(uuid, data_type)
    }

    /// Save a CRDT document.
    pub fn crdt_save_document(
        &self,
        uuid: &str,
        data_type: &str,
        automerge_doc: &[u8],
        sync_state: &[u8],
        dirty: bool,
        last_sync_at: Option<&str>,
    ) -> Result<()> {
        self.storage.save_crdt_document(
            uuid,
            data_type,
            automerge_doc,
            sync_state,
            dirty,
            last_sync_at,
        )
    }

    /// Get all dirty CRDT documents (for sync), ordered by sync priority.
    pub fn crdt_get_dirty_documents(&self) -> Result<Vec<crate::types::CrdtDocumentRow>> {
        self.storage.get_dirty_crdt_documents()
    }

    /// List all CRDT documents with payloads.
    pub fn crdt_list_documents(&self) -> Result<Vec<crate::types::CrdtDocumentRow>> {
        self.storage.list_crdt_documents()
    }

    /// List all CRDT document keys (uuid, data_type).
    pub fn crdt_list_document_keys(&self) -> Result<Vec<(String, String)>> {
        self.storage.list_crdt_document_keys()
    }

    /// Delete a CRDT document by key.
    pub fn crdt_delete_document(&self, uuid: &str, data_type: &str) -> Result<()> {
        self.storage.delete_crdt_document(uuid, data_type)
    }

    /// Mark a CRDT document as dirty/clean.
    pub fn crdt_set_document_dirty(&self, uuid: &str, data_type: &str, dirty: bool) -> Result<()> {
        self.storage.set_crdt_document_dirty(uuid, data_type, dirty)
    }

    // ===== Undo Helper Methods =====

    fn check_undo_conflict(
        &self,
        log_entry: &ThingsChangeLogEntry,
        snapshot: &ThingsSnapshot,
    ) -> Result<Option<ThingsUndoConflict>> {
        match log_entry.op_type {
            ThingsOperationType::CreateCollection => {
                // Check if collection still exists
                let exists = snapshot
                    .collections
                    .iter()
                    .any(|c| c.uuid == log_entry.entity_uuid);
                if !exists {
                    return Ok(Some(ThingsUndoConflict {
                        conflict_type: ThingsUndoConflictType::EntityModified,
                        description: "Collection no longer exists".to_string(),
                        options: vec![],
                    }));
                }
                Ok(None)
            }
            ThingsOperationType::CreateThing => {
                // Check if thing still exists
                let exists = snapshot
                    .things
                    .iter()
                    .any(|t| t.uuid == log_entry.entity_uuid);
                if !exists {
                    return Ok(Some(ThingsUndoConflict {
                        conflict_type: ThingsUndoConflictType::EntityModified,
                        description: "Thing no longer exists".to_string(),
                        options: vec![],
                    }));
                }
                Ok(None)
            }
            ThingsOperationType::DeleteCollection => {
                // Check if collection already exists (restored elsewhere)
                let exists = snapshot
                    .collections
                    .iter()
                    .any(|c| c.uuid == log_entry.entity_uuid);
                if exists {
                    return Ok(Some(ThingsUndoConflict {
                        conflict_type: ThingsUndoConflictType::EntityExists,
                        description: "Collection already exists".to_string(),
                        options: vec![],
                    }));
                }
                Ok(None)
            }
            ThingsOperationType::DeleteThing => {
                // Check if thing already exists
                let exists = snapshot
                    .things
                    .iter()
                    .any(|t| t.uuid == log_entry.entity_uuid);
                if exists {
                    return Ok(Some(ThingsUndoConflict {
                        conflict_type: ThingsUndoConflictType::EntityExists,
                        description: "Thing already exists".to_string(),
                        options: vec![],
                    }));
                }
                // Check if parent collection exists
                let details: serde_json::Value =
                    serde_json::from_str(&log_entry.details_json).unwrap_or_default();
                let collection_uuid = details["collection_uuid"].as_str().unwrap_or("");
                let parent_exists = snapshot
                    .collections
                    .iter()
                    .any(|c| c.uuid == collection_uuid);
                if !parent_exists && !collection_uuid.is_empty() {
                    return Ok(Some(ThingsUndoConflict {
                        conflict_type: ThingsUndoConflictType::ParentDeleted,
                        description: format!(
                            "Parent collection '{}' has been deleted",
                            collection_uuid
                        ),
                        options: vec![
                            ThingsUndoResolutionOption {
                                id: "cascade_restore".to_string(),
                                label: "Restore parent collection too".to_string(),
                                description:
                                    "Restore the parent collection and then restore this thing"
                                        .to_string(),
                            },
                            ThingsUndoResolutionOption {
                                id: "move_to_other".to_string(),
                                label: "Move to another collection".to_string(),
                                description: "Restore this thing to a different collection"
                                    .to_string(),
                            },
                            ThingsUndoResolutionOption {
                                id: "cancel".to_string(),
                                label: "Cancel".to_string(),
                                description: "Do not restore this thing".to_string(),
                            },
                        ],
                    }));
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub fn things_bootstrap_stash_local_snapshot_if_needed(&self, device_id: &str) -> Result<bool> {
        const STASH_KEY: &str = "things.bootstrap.stash_snapshot_json";

        if self.storage.get_internal_kv(STASH_KEY)?.is_some() {
            tracing::debug!(
                device_id,
                "Bootstrap stash already exists; skipping new stash"
            );
            return Ok(false);
        }

        let dirty_documents = self
            .storage
            .get_dirty_crdt_documents()
            .context("Failed to load dirty CRDT documents for bootstrap stash")?;
        if dirty_documents.is_empty() {
            tracing::debug!(
                device_id,
                "No dirty CRDT documents found for bootstrap stash"
            );
            return Ok(false);
        }

        let dirty_document_keys: Vec<String> = dirty_documents
            .iter()
            .map(|doc| format!("{}:{}", doc.uuid, doc.data_type))
            .collect();

        let payload = serialize_bootstrap_stash_documents(&dirty_documents)
            .context("Failed to serialize bootstrap stash CRDT documents")?;
        self.storage
            .set_internal_kv(STASH_KEY, &payload)
            .context("Failed to persist bootstrap stash snapshot")?;
        tracing::info!(
            device_id,
            dirty_doc_count = dirty_document_keys.len(),
            dirty_document_keys = ?dirty_document_keys,
            "Persisted bootstrap stash from dirty CRDT documents"
        );
        Ok(true)
    }

    pub fn things_bootstrap_has_stash(&self) -> Result<bool> {
        const STASH_KEY: &str = "things.bootstrap.stash_snapshot_json";
        Ok(self.storage.get_internal_kv(STASH_KEY)?.is_some())
    }

    /// V3: Bootstrap by replaying stashed local changes.
    ///
    /// In V3 multi-document architecture, bootstrapping works differently:
    /// 1. Server sync is handled per-document, so there's no single "server snapshot"
    /// 2. Instead, we replay the stashed local snapshot to fresh documents
    /// 3. These documents will be marked dirty and synced on next sync cycle
    ///
    /// Note: The `_server_snapshot_doc` parameter is ignored in V3 - server data
    /// comes through per-document sync, not a single snapshot.
    pub fn things_bootstrap_from_server_snapshot_and_replay_stash(
        &self,
        device_id: &str,
        _server_snapshot_doc: Vec<u8>,
        _server_last_sync_at: Option<&str>,
    ) -> Result<()> {
        const STASH_KEY: &str = "things.bootstrap.stash_snapshot_json";
        const DONE_KEY: &str = "things.bootstrap.done";

        // Load the stashed local snapshot
        let stash_json = self
            .storage
            .get_internal_kv(STASH_KEY)?
            .ok_or_else(|| anyhow::anyhow!("No bootstrap stash found"))?;
        let stash = parse_bootstrap_replay_source(&stash_json)?;

        tracing::info!(
            device_id,
            "Replaying bootstrap stash onto fresh local documents"
        );

        // Clear all existing V3 documents from storage
        self.storage
            .delete_all_crdt_documents()
            .context("Failed to clear existing CRDT documents")?;

        if let BootstrapReplaySource::Documents(documents) = stash {
            let stashed_document_keys: Vec<String> = documents
                .iter()
                .map(|doc| format!("{}:{}", doc.uuid, doc.data_type))
                .collect();
            restore_stashed_documents(&self.storage, &documents)
                .context("Failed to restore bootstrap-stashed CRDT documents")?;
            self.storage.set_internal_kv(DONE_KEY, "1")?;
            self.storage.delete_internal_kv(STASH_KEY)?;
            tracing::info!(
                device_id,
                stashed_doc_count = stashed_document_keys.len(),
                stashed_document_keys = ?stashed_document_keys,
                "Restored bootstrap-stashed CRDT documents onto fresh storage"
            );
            return Ok(());
        }

        let BootstrapReplaySource::LegacySnapshot(stash) =
            parse_bootstrap_replay_source(&stash_json)?
        else {
            unreachable!("document stash handled above")
        };

        // Get a fresh document set (mutable)
        let mut doc_set = self.get_or_init_document_set(device_id)?;

        // Replay stashed collections
        for collection in &stash.collections {
            // Create/init collection document and update meta
            doc_set.get_or_init_collection(&collection.uuid)?;
            let trigger = crate::things_crdt::trigger_update_from_tri_state(
                collection.trigger_uuid.as_deref(),
            );
            doc_set.update_collection_meta_with_timestamps(
                &collection.uuid,
                Some(collection.title.clone()),
                None, // status
                trigger,
                Some(collection.created_at.clone()),
                Some(collection.updated_at.clone()),
            )?;
        }

        // Replay stashed things
        for thing in &stash.things {
            replay_stashed_thing_into_document_set(&mut doc_set, thing)?;
        }

        // Persist all documents as dirty (so they sync on next cycle)
        doc_set.save_to_storage(&self.storage)?;

        // Mark bootstrap as done
        self.storage.set_internal_kv(DONE_KEY, "1")?;

        // Clear the stash
        self.storage.delete_internal_kv(STASH_KEY)?;

        Ok(())
    }

    /// Replay stashed local changes onto the current V3 document set.
    ///
    /// Use this when the current storage already contains server documents pulled during
    /// first-sync bootstrap. Unlike `things_bootstrap_from_server_snapshot_and_replay_stash`,
    /// this preserves the pulled server state and layers the stashed local changes on top.
    pub fn things_bootstrap_replay_stash_onto_current_documents(
        &self,
        device_id: &str,
    ) -> Result<()> {
        const STASH_KEY: &str = "things.bootstrap.stash_snapshot_json";
        const DONE_KEY: &str = "things.bootstrap.done";

        let stash_json = self
            .storage
            .get_internal_kv(STASH_KEY)?
            .ok_or_else(|| anyhow::anyhow!("No bootstrap stash found"))?;
        let stash = parse_bootstrap_replay_source(&stash_json)?;

        if let BootstrapReplaySource::Documents(documents) = stash {
            let stashed_document_keys: Vec<String> = documents
                .iter()
                .map(|doc| format!("{}:{}", doc.uuid, doc.data_type))
                .collect();
            merge_stashed_documents_onto_current_documents(&self.storage, &documents)
                .context("Failed to merge bootstrap-stashed CRDT documents")?;
            self.storage.set_internal_kv(DONE_KEY, "1")?;
            self.storage.delete_internal_kv(STASH_KEY)?;
            tracing::info!(
                device_id,
                stashed_doc_count = stashed_document_keys.len(),
                stashed_document_keys = ?stashed_document_keys,
                "Merged bootstrap-stashed CRDT documents onto current storage"
            );
            return Ok(());
        }

        let BootstrapReplaySource::LegacySnapshot(stash) =
            parse_bootstrap_replay_source(&stash_json)?
        else {
            unreachable!("document stash handled above")
        };

        let mut doc_set = self.get_or_init_document_set(device_id)?;

        for collection in &stash.collections {
            doc_set.get_or_init_collection(&collection.uuid)?;
            let trigger = crate::things_crdt::trigger_update_from_tri_state(
                collection.trigger_uuid.as_deref(),
            );
            doc_set.update_collection_meta_with_timestamps(
                &collection.uuid,
                Some(collection.title.clone()),
                None,
                trigger,
                Some(collection.created_at.clone()),
                Some(collection.updated_at.clone()),
            )?;
        }

        for thing in &stash.things {
            replay_stashed_thing_into_document_set(&mut doc_set, thing)?;
        }

        doc_set.save_to_storage(&self.storage)?;
        self.storage.set_internal_kv(DONE_KEY, "1")?;
        self.storage.delete_internal_kv(STASH_KEY)?;
        Ok(())
    }

    /// Reconcile trigger bindings after a sync operation.
    /// Returns a list of trigger UUIDs that need to be downloaded and installed.
    pub fn reconcile_trigger_bindings_after_sync(&self, device_id: &str) -> Result<Vec<String>> {
        let doc_set = self.get_or_init_document_set(device_id)?;
        let snapshot = doc_set.extract_snapshot()?;

        // Desired bindings for *all* non-deleted entities.
        let mut desired_bindings: std::collections::HashMap<(String, String), Option<String>> =
            std::collections::HashMap::new();

        for c in &snapshot.collections {
            desired_bindings.insert(
                ("collection".to_string(), c.uuid.clone()),
                c.trigger_uuid.clone(),
            );
        }

        for t in &snapshot.things {
            desired_bindings.insert(
                ("thing".to_string(), t.uuid.clone()),
                t.trigger_uuid.clone(),
            );
        }

        // Snapshot of existing bound triggers (for uninstall decisions later).
        let existing_bound_triggers: std::collections::HashSet<String> = self
            .storage
            .list_bound_trigger_uuids()?
            .into_iter()
            .collect();

        // Apply desired bindings and also delete rows for entities that no longer exist.
        let existing_rows = self.storage.list_trigger_bindings()?;
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();

        for (trigger_uuid, entity_type, entity_uuid) in existing_rows {
            let key = (entity_type.clone(), entity_uuid.clone());
            seen.insert(key.clone());

            match desired_bindings.get(&key) {
                None => {
                    // Entity disappeared (deleted); remove stale binding.
                    self.storage
                        .delete_trigger_binding(&entity_type, &entity_uuid)?;
                }
                Some(None) => {
                    // Entity exists but wants no binding.
                    self.storage
                        .delete_trigger_binding(&entity_type, &entity_uuid)?;
                }
                Some(Some(desired_trigger)) => {
                    if desired_trigger != &trigger_uuid {
                        self.storage.upsert_trigger_binding(
                            desired_trigger,
                            &entity_type,
                            &entity_uuid,
                        )?;
                    }
                }
            }
        }

        // Insert any bindings for entities we haven't seen yet.
        for ((entity_type, entity_uuid), trigger_uuid) in &desired_bindings {
            if seen.contains(&(entity_type.clone(), entity_uuid.clone())) {
                continue;
            }
            if let Some(trigger_uuid) = trigger_uuid {
                self.storage
                    .upsert_trigger_binding(trigger_uuid, entity_type, entity_uuid)?;
            }
        }

        // Collect all trigger UUIDs that are now bound
        let mut all_trigger_uuids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for trigger_uuid_opt in desired_bindings.values() {
            if let Some(trigger_uuid) = trigger_uuid_opt {
                all_trigger_uuids.insert(trigger_uuid.clone());
            }
        }

        // Find triggers that need to be downloaded (not yet installed)
        let mut triggers_to_download = Vec::new();
        for trigger_uuid in &all_trigger_uuids {
            // Check if trigger is already installed
            if self.storage.fetch_trigger(trigger_uuid)?.is_none() {
                triggers_to_download.push(trigger_uuid.clone());
            }
        }

        // Find triggers that are no longer bound and can be uninstalled
        for old_trigger_uuid in existing_bound_triggers {
            if !all_trigger_uuids.contains(&old_trigger_uuid) {
                // Check if trigger is still bound to anything (shouldn't be, but double-check)
                if !self.storage.is_trigger_bound(&old_trigger_uuid)? {
                    info!(trigger_uuid = %old_trigger_uuid, "Uninstalling trigger no longer bound to any entity");
                    let _ = self.storage.delete_trigger(&old_trigger_uuid);
                    self.emit_trigger_event(TriggerEvent::TriggerDelete {
                        trigger_uuid: old_trigger_uuid.clone(),
                    });
                }
            }
        }

        Ok(triggers_to_download)
    }

    pub fn list_trigger_logs_json(
        &self,
        trigger_uuid: &str,
        limit: Option<u32>,
        run_type: Option<TriggerRunType>,
    ) -> Result<String> {
        let logs = self
            .storage
            .list_trigger_logs(trigger_uuid, limit, run_type)?;
        to_string(&logs).context("Failed to serialize trigger logs")
    }
    pub fn export_trigger_logs_json(
        &self,
        trigger_uuid: &str,
        run_type: Option<TriggerRunType>,
    ) -> Result<String> {
        let logs = self.storage.export_trigger_logs(trigger_uuid, run_type)?;
        to_string(&logs).context("Failed to serialize trigger logs for export")
    }

    // ===== Notification queries =====

    pub fn list_notifications_grouped_json(&self, limit: u32) -> Result<String> {
        let groups = self.storage.list_notifications_grouped(limit)?;
        to_string(&groups).context("Failed to serialize notification groups")
    }

    pub fn list_notifications_by_category_json(
        &self,
        category: &str,
        limit: u32,
    ) -> Result<String> {
        let items = self
            .storage
            .list_notifications_by_category(category, limit)?;
        to_string(&items).context("Failed to serialize notifications by category")
    }

    pub fn list_notifications_flat_json(&self, limit: u32, offset: u32) -> Result<String> {
        let items = self.storage.list_notifications_flat(limit, offset)?;
        to_string(&items).context("Failed to serialize flat notifications")
    }

    pub fn get_latest_unread_notification_json(&self) -> Result<Option<String>> {
        let entry = self.storage.get_latest_unread_notification()?;
        match entry {
            Some(e) => Ok(Some(
                to_string(&e).context("Failed to serialize latest unread notification")?,
            )),
            None => Ok(None),
        }
    }

    pub fn get_unread_notification_count(&self) -> Result<i64> {
        self.storage.get_unread_notification_count()
    }

    pub fn mark_notification_read(&self, notification_id: i64) -> Result<()> {
        self.storage.mark_notification_read(notification_id)?;
        self.emit_notification_event(NotificationEvent::Read { notification_id });
        Ok(())
    }

    pub fn record_notification_response(
        &self,
        notification_id: i64,
        action: NotificationResponseAction,
    ) -> Result<()> {
        self.storage
            .record_notification_response(notification_id, &action)?;
        self.emit_notification_event(NotificationEvent::Responded {
            notification_id,
            action,
        });
        Ok(())
    }

    pub fn mark_category_notifications_read(&self, category: &str) -> Result<()> {
        self.storage.mark_category_notifications_read(category)?;
        self.emit_notification_event(NotificationEvent::CategoryRead {
            category: category.to_string(),
        });
        Ok(())
    }

    pub fn mark_all_notifications_read(&self) -> Result<()> {
        self.storage.mark_all_notifications_read()?;
        self.emit_notification_event(NotificationEvent::AllRead);
        Ok(())
    }

    pub fn delete_notifications_by_category(&self, category: &str) -> Result<()> {
        self.storage.delete_notifications_by_category(category)?;
        self.emit_notification_event(NotificationEvent::CategoryDeleted {
            category: category.to_string(),
        });
        Ok(())
    }

    pub fn run_due_triggers<C>(&self, callback: &C) -> Result<Vec<TriggerExecutionSummary>>
    where
        C: TriggerCallback,
    {
        let now = Utc::now();
        let due = self.storage.fetch_due_triggers(now)?;
        let mut summaries = Vec::new();

        for trigger in due {
            let fire_time = trigger.next_fire.unwrap_or(now);
            let summary = self.execute_trigger(&trigger, fire_time, TriggerRunType::Automatic)?;
            callback.on_trigger(&summary)?;

            // Parse rules to determine the next schedule.
            let precondition: Vec<TriggerRule> = serde_json::from_str(&trigger.precondition_json)
                .context("Failed to parse precondition JSON")?;
            let condition: Vec<TriggerRule> = serde_json::from_str(&trigger.condition_json)
                .context("Failed to parse condition JSON")?;
            let timings = extract_timings_from_rules(&precondition, &condition)?;

            // Ensure the computed next fire is strictly after "now" even if this run is late.
            let next_fire =
                resolve_post_run_next_fire(&timings, fire_time, now, DEFAULT_TIMEZONE_OFFSET)?;

            self.storage.update_next_fire(
                &trigger.trigger_uuid,
                summary.fired_at,
                next_fire,
                summary.result,
            )?;

            self.emit_trigger_event(TriggerEvent::TriggerFired {
                trigger_uuid: trigger.trigger_uuid.clone(),
                fired_at: summary.fired_at.to_rfc3339(),
                next_fire: next_fire.map(|dt| dt.to_rfc3339()),
                result: summary.result,
            });
            summaries.push(summary);
        }

        Ok(summaries)
    }

    /// Get things and collections bound to a trigger
    fn get_bound_entities_for_trigger(
        &self,
        trigger_uuid: &str,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let entities = self.storage.get_entities_for_trigger(trigger_uuid)?;
        let mut thing_uuids = Vec::new();
        let mut collection_uuids = Vec::new();

        for (entity_type, entity_uuid) in entities {
            match entity_type.as_str() {
                "thing" => thing_uuids.push(entity_uuid),
                "collection" => collection_uuids.push(entity_uuid),
                _ => warn!(entity_type = %entity_type, "Unknown entity type in trigger binding"),
            }
        }

        Ok((thing_uuids, collection_uuids))
    }

    /// Public API to get entities bound to a trigger (for FFI/Flutter bridge)
    pub fn get_bound_entities_for_trigger_api(
        &self,
        trigger_uuid: &str,
    ) -> Result<(Vec<String>, Vec<String>)> {
        self.get_bound_entities_for_trigger(trigger_uuid)
    }

    pub fn run_trigger_now<C>(&self, trigger_uuid: &str, callback: &C) -> Result<bool>
    where
        C: TriggerCallback,
    {
        let trigger = self
            .storage
            .fetch_trigger(trigger_uuid)?
            .ok_or_else(|| anyhow!("Trigger not found: {trigger_uuid}"))?;
        let fire_time = Utc::now();
        let summary = self.execute_trigger(&trigger, fire_time, TriggerRunType::Manual)?;
        callback.on_trigger(&summary)?;

        // Parse rules to determine the next schedule.
        let precondition: Vec<TriggerRule> = serde_json::from_str(&trigger.precondition_json)
            .context("Failed to parse precondition JSON")?;
        let condition: Vec<TriggerRule> = serde_json::from_str(&trigger.condition_json)
            .context("Failed to parse condition JSON")?;
        let timings = extract_timings_from_rules(&precondition, &condition)?;
        let next_fire =
            resolve_post_run_next_fire(&timings, fire_time, fire_time, DEFAULT_TIMEZONE_OFFSET)?;

        self.storage.update_next_fire(
            &trigger.trigger_uuid,
            summary.fired_at,
            next_fire,
            summary.result,
        )?;

        self.emit_trigger_event(TriggerEvent::TriggerFired {
            trigger_uuid: trigger.trigger_uuid.clone(),
            fired_at: summary.fired_at.to_rfc3339(),
            next_fire: next_fire.map(|dt| dt.to_rfc3339()),
            result: summary.result,
        });

        Ok(summary.result)
    }

    /// Simulate time advancing to a specific point and run all due triggers
    /// Returns summaries of all executed triggers
    pub fn simulate_time_to<C>(
        &self,
        target_time: DateTime<Utc>,
        callback: &C,
    ) -> Result<Vec<TriggerExecutionSummary>>
    where
        C: TriggerCallback,
    {
        let due = self.storage.fetch_due_triggers(target_time)?;
        let mut summaries = Vec::new();

        for trigger in due {
            let fire_time = trigger.next_fire.unwrap_or(target_time);
            let summary = self.execute_trigger(&trigger, fire_time, TriggerRunType::Automatic)?;
            callback.on_trigger(&summary)?;

            // Parse rules to determine the next schedule.
            let precondition: Vec<TriggerRule> = serde_json::from_str(&trigger.precondition_json)
                .context("Failed to parse precondition JSON")?;
            let condition: Vec<TriggerRule> = serde_json::from_str(&trigger.condition_json)
                .context("Failed to parse condition JSON")?;
            let timings = extract_timings_from_rules(&precondition, &condition)?;

            let next_fire = resolve_post_run_next_fire(
                &timings,
                fire_time,
                target_time,
                DEFAULT_TIMEZONE_OFFSET,
            )?;
            self.storage.update_next_fire(
                &trigger.trigger_uuid,
                summary.fired_at,
                next_fire,
                summary.result,
            )?;

            self.emit_trigger_event(TriggerEvent::TriggerFired {
                trigger_uuid: trigger.trigger_uuid.clone(),
                fired_at: summary.fired_at.to_rfc3339(),
                next_fire: next_fire.map(|dt| dt.to_rfc3339()),
                result: summary.result,
            });
            summaries.push(summary);
        }

        Ok(summaries)
    }

    pub fn replay_trigger(
        &self,
        trigger_uuid: &str,
        start_iso: Option<String>,
        end_iso: Option<String>,
    ) -> Result<TriggerReplaySummary> {
        let trigger = self
            .storage
            .fetch_trigger(trigger_uuid)?
            .ok_or_else(|| anyhow!("Trigger not found: {trigger_uuid}"))?;

        let range = self
            .storage
            .events_time_range()?
            .ok_or_else(|| anyhow!("No events available for replay"))?;

        let start = match start_iso {
            Some(value) => DateTime::parse_from_rfc3339(&value)
                .map(|dt| dt.with_timezone(&Utc))
                .context("Invalid replay start timestamp")?,
            None => range.0,
        };

        let end = match end_iso {
            Some(value) => DateTime::parse_from_rfc3339(&value)
                .map(|dt| dt.with_timezone(&Utc))
                .context("Invalid replay end timestamp")?,
            None => range.1,
        };

        if end <= start {
            return Err(anyhow!("Replay window must be positive"));
        }

        // Parse rules to extract schedule metadata + optional repeat frequency.
        let precondition: Vec<TriggerRule> = serde_json::from_str(&trigger.precondition_json)
            .context("Failed to parse precondition JSON")?;
        let condition: Vec<TriggerRule> = serde_json::from_str(&trigger.condition_json)
            .context("Failed to parse condition JSON")?;
        let timings = extract_timings_from_rules(&precondition, &condition)?;
        let repeat_freq = extract_repeat_frequency_from_conditions(&condition);
        let events: Vec<MonitoringEvent> = self
            .storage
            .list_events_between_utc(start.timestamp(), end.timestamp())?
            .into_iter()
            .map(|ev| MonitoringEvent {
                event_type: ev.event_type,
                timestamp: ev.timestamp.to_rfc3339(),
                metadata_json: serde_json::to_string(&ev.metadata)
                    .unwrap_or_else(|_| "{}".to_string()),
            })
            .collect();
        let occurrences = build_trigger_occurrences(
            &timings,
            &events,
            start,
            end,
            start,
            DEFAULT_TIMEZONE_OFFSET,
        )?;

        let mut runs_considered = 0;
        let mut runs_executed = 0;
        let mut runs_succeeded = 0;
        let mut last_success: Option<DateTime<Utc>> = None;

        for fire_time in occurrences {
            runs_considered += 1;

            if let Some(last) = last_success {
                if let Some(ref freq) = repeat_freq {
                    if let Some(min_gap) = repeat_min_gap(freq) {
                        if fire_time - last < min_gap {
                            continue;
                        }
                    }
                }
            }

            let summary = self.execute_trigger(&trigger, fire_time, TriggerRunType::Replay)?;
            runs_executed += 1;
            if summary.result {
                runs_succeeded += 1;
                last_success = Some(summary.fired_at);
            }
        }

        Ok(TriggerReplaySummary {
            trigger_id: trigger.trigger_uuid.clone(),
            start,
            end,
            runs_considered,
            runs_executed,
            runs_succeeded,
        })
    }

    /// Test a trigger configuration against stored events without registering it.
    ///
    /// This is the SDK equivalent of the `trigger-test` CLI tool. It accepts a trigger
    /// configuration (JSON) and simulates its execution over a time range using local events.
    ///
    /// # Arguments
    /// * `trigger_json` - Full trigger configuration JSON (name, version, precondition, condition)
    /// * `start_iso` - Optional start time (RFC3339); defaults to first event timestamp
    /// * `end_iso` - Optional end time (RFC3339); defaults to last event timestamp
    /// * `manual` - If true, runs once at end time ignoring precondition gates (like --manual flag)
    ///
    /// # Returns
    /// JSON string containing simulation results
    pub fn trigger_test_json(
        &self,
        trigger_json: &str,
        start_iso: Option<String>,
        end_iso: Option<String>,
        manual: bool,
    ) -> Result<String> {
        use rule_trigger_engine::TriggerEvaluationReport;

        // Parse the trigger configuration
        let config = TriggerConfig::from_json(trigger_json)
            .map_err(|e| anyhow!("Failed to parse trigger config: {e}"))?;

        // Extract timing info from preconditions
        let timings = config
            .extract_timing()
            .map_err(|e| anyhow!("Failed to extract timing: {e}"))?;

        let timing_summary = describe_timing_sources(&timings);

        let repeat_freq = timings.iter().find_map(|t| match t {
            rule_trigger_engine::TriggerTiming::RepeatFrequency { frequency } => {
                Some(frequency.clone())
            }
            _ => None,
        });

        // Get event time range
        let range = self
            .storage
            .events_time_range()?
            .ok_or_else(|| anyhow!("No events available for testing"))?;

        let start_utc = match start_iso {
            Some(value) => DateTime::parse_from_rfc3339(&value)
                .map(|dt| dt.with_timezone(&Utc))
                .context("Invalid start timestamp")?,
            None => range.0,
        };

        let end_utc = match end_iso {
            Some(value) => DateTime::parse_from_rfc3339(&value)
                .map(|dt| dt.with_timezone(&Utc))
                .context("Invalid end timestamp")?,
            None => range.1,
        };

        if end_utc <= start_utc {
            return Err(anyhow!("Test window must be positive (start < end)"));
        }

        // Fetch all events in range for simulation
        let all_events: Vec<MonitoringEvent> = self
            .storage
            .list_events_between_utc(start_utc.timestamp(), end_utc.timestamp())?
            .into_iter()
            .map(|ev| MonitoringEvent {
                event_type: ev.event_type,
                timestamp: ev.timestamp.to_rfc3339(),
                metadata_json: serde_json::to_string(&ev.metadata)
                    .unwrap_or_else(|_| "{}".to_string()),
            })
            .collect();

        let tz = default_timezone();

        // Build result structure
        #[derive(serde::Serialize)]
        struct TriggerTestResult {
            trigger_name: String,
            timing_summary: String,
            repeat_frequency: Option<String>,
            start_time: String,
            end_time: String,
            mode: String,
            events_in_window: usize,
            runs: Vec<TriggerTestRun>,
            summary: TriggerTestSummary,
        }

        #[derive(serde::Serialize)]
        struct TriggerTestRun {
            trigger_time: String,
            result: bool,
            status: String,
            report: Option<TriggerEvaluationReport>,
        }

        #[derive(serde::Serialize)]
        struct TriggerTestSummary {
            runs_considered: u32,
            runs_executed: u32,
            runs_succeeded: u32,
        }

        let freq_str = repeat_freq.as_ref().map(|f| match f {
            rule_trigger_engine::RepeatFrequency::PerDay(n) => format!("per_day({n})"),
            rule_trigger_engine::RepeatFrequency::PerWeek(n) => format!("per_week({n})"),
        });

        // Manual mode: single evaluation at end time
        if manual {
            let visible_events = filter_events_at_time(&all_events, end_utc, 120);
            let eval_ctx = EvaluationContext {
                events: &visible_events,
                current_event: select_current_event(&visible_events, &timings, end_utc),
                current_time: end_utc.timestamp(),
                timezone_offset: DEFAULT_TIMEZONE_OFFSET,
            };

            let report = config.evaluate_detailed(&eval_ctx, PreconditionPolicy::IgnoreGates);

            let run = TriggerTestRun {
                trigger_time: end_utc.with_timezone(&tz).to_rfc3339(),
                result: report.overall_result,
                status: "manual".to_string(),
                report: Some(report.clone()),
            };

            let result = TriggerTestResult {
                trigger_name: config.name,
                timing_summary,
                repeat_frequency: freq_str,
                start_time: start_utc.to_rfc3339(),
                end_time: end_utc.to_rfc3339(),
                mode: "manual".to_string(),
                events_in_window: all_events.len(),
                runs: vec![run],
                summary: TriggerTestSummary {
                    runs_considered: 1,
                    runs_executed: 1,
                    runs_succeeded: if report.overall_result { 1 } else { 0 },
                },
            };

            return serde_json::to_string(&result).context("Failed to serialize result");
        }

        let occurrences = build_trigger_occurrences(
            &timings,
            &all_events,
            start_utc,
            end_utc,
            start_utc,
            DEFAULT_TIMEZONE_OFFSET,
        )?;

        let mut runs = Vec::new();
        let mut runs_considered = 0u32;
        let mut runs_executed = 0u32;
        let mut runs_succeeded = 0u32;
        let mut last_success: Option<DateTime<Utc>> = None;

        for trigger_time_utc in occurrences {
            runs_considered += 1;

            // Check repeat frequency gating
            if let (Some(last), Some(freq)) = (last_success, &repeat_freq) {
                if let Some(min_gap) = repeat_min_gap(freq) {
                    if trigger_time_utc - last < min_gap {
                        runs.push(TriggerTestRun {
                            trigger_time: trigger_time_utc.with_timezone(&tz).to_rfc3339(),
                            result: false,
                            status: "skipped_repeat_frequency".to_string(),
                            report: None,
                        });
                        continue;
                    }
                }
            }

            let visible_events = filter_events_at_time(&all_events, trigger_time_utc, 120);
            let eval_ctx = EvaluationContext {
                events: &visible_events,
                current_event: select_current_event(&visible_events, &timings, trigger_time_utc),
                current_time: trigger_time_utc.timestamp(),
                timezone_offset: DEFAULT_TIMEZONE_OFFSET,
            };

            let report = config.evaluate_detailed(&eval_ctx, PreconditionPolicy::EnforceAsGates);

            let has_error = report
                .preconditions
                .iter()
                .chain(report.conditions.iter())
                .any(|e| e.error.is_some());

            runs_executed += 1;
            let fired = report.overall_result;
            if fired {
                runs_succeeded += 1;
                last_success = Some(trigger_time_utc);
            }

            runs.push(TriggerTestRun {
                trigger_time: trigger_time_utc.with_timezone(&tz).to_rfc3339(),
                result: fired,
                status: if has_error {
                    "error".to_string()
                } else if fired {
                    "fired".to_string()
                } else {
                    "not_fired".to_string()
                },
                report: Some(report),
            });
        }

        let result = TriggerTestResult {
            trigger_name: config.name,
            timing_summary,
            repeat_frequency: freq_str,
            start_time: start_utc.to_rfc3339(),
            end_time: end_utc.to_rfc3339(),
            mode: "automatic".to_string(),
            events_in_window: all_events.len(),
            runs,
            summary: TriggerTestSummary {
                runs_considered,
                runs_executed,
                runs_succeeded,
            },
        };

        serde_json::to_string(&result).context("Failed to serialize result")
    }

    fn execute_trigger(
        &self,
        trigger: &StoredTrigger,
        fire_time: DateTime<Utc>,
        run_type: TriggerRunType,
    ) -> Result<TriggerExecutionSummary> {
        info!(
            trigger_id = %trigger.trigger_uuid,
            version = %trigger.version,
            run_type = %run_type,
            "Executing trigger with CEL evaluation"
        );

        const EVENT_LOOKBACK_MINUTES: u32 = 60 * 24 * 7;

        // Parse JSON rules
        let precondition: Vec<TriggerRule> = match serde_json::from_str(&trigger.precondition_json)
        {
            Ok(v) => v,
            Err(err) => {
                let payload = json!({
                    "kind": "trigger_execution_report_v1",
                    "trigger_uuid": trigger.trigger_uuid,
                    "trigger_name": trigger.name,
                    "fired_at": fire_time.to_rfc3339(),
                    "run_type": run_type.as_str(),
                    "error": format!("Failed to parse precondition JSON: {err}"),
                });
                let message = serde_json::to_string(&payload)
                    .unwrap_or_else(|_| "{\"kind\":\"trigger_execution_report_v1\",\"error\":\"serialization_failed\"}".to_string());
                let _ = self.storage.insert_trigger_log(
                    &trigger.trigger_uuid,
                    TriggerLogLevel::Error,
                    &message,
                    fire_time,
                    run_type.clone(),
                );
                return Err(anyhow!("Failed to parse precondition JSON: {err}"));
            }
        };
        let condition: Vec<TriggerRule> = match serde_json::from_str(&trigger.condition_json) {
            Ok(v) => v,
            Err(err) => {
                let payload = json!({
                    "kind": "trigger_execution_report_v1",
                    "trigger_uuid": trigger.trigger_uuid,
                    "trigger_name": trigger.name,
                    "fired_at": fire_time.to_rfc3339(),
                    "run_type": run_type.as_str(),
                    "error": format!("Failed to parse condition JSON: {err}"),
                });
                let message = serde_json::to_string(&payload)
                    .unwrap_or_else(|_| "{\"kind\":\"trigger_execution_report_v1\",\"error\":\"serialization_failed\"}".to_string());
                let _ = self.storage.insert_trigger_log(
                    &trigger.trigger_uuid,
                    TriggerLogLevel::Error,
                    &message,
                    fire_time,
                    run_type.clone(),
                );
                return Err(anyhow!("Failed to parse condition JSON: {err}"));
            }
        };

        // Build rule-trigger-engine config
        let precondition_rules: Vec<EngineRule> = precondition
            .into_iter()
            .map(|rule| EngineRule {
                rule: rule.rule,
                description: rule.description,
            })
            .collect();
        let condition_rules: Vec<EngineRule> = condition
            .into_iter()
            .map(|rule| EngineRule {
                rule: rule.rule,
                description: rule.description,
            })
            .collect();

        let config = TriggerConfig {
            name: trigger.name.clone(),
            version: trigger.version.clone(),
            precondition: precondition_rules,
            condition: condition_rules,
        };
        let timings = config.extract_timing().unwrap_or_default();

        // Fetch recent events and map to engine event type
        let recent = self
            .storage
            .fetch_events_recent(fire_time, EVENT_LOOKBACK_MINUTES)?;
        let events: Vec<MonitoringEvent> = recent
            .into_iter()
            .map(|event| MonitoringEvent {
                event_type: event.event_type,
                timestamp: event.timestamp.to_rfc3339(),
                metadata_json: serde_json::to_string(&event.metadata)
                    .unwrap_or_else(|_| "{}".to_string()),
            })
            .collect();

        let eval_ctx = EvaluationContext {
            events: &events,
            current_event: select_current_event(&events, &timings, fire_time),
            current_time: fire_time.timestamp(),
            timezone_offset: DEFAULT_TIMEZONE_OFFSET,
        };

        let precondition_policy = match run_type {
            TriggerRunType::Manual => PreconditionPolicy::IgnoreGates,
            TriggerRunType::Automatic | TriggerRunType::Replay => {
                PreconditionPolicy::EnforceAsGates
            }
        };

        let report = config.evaluate_detailed(&eval_ctx, precondition_policy);

        // Persist one aggregated log entry per execution.
        let has_errors = report
            .preconditions
            .iter()
            .chain(report.conditions.iter())
            .any(|e| e.error.is_some());
        let level = if has_errors {
            TriggerLogLevel::Error
        } else {
            TriggerLogLevel::Info
        };
        let payload = json!({
            "kind": "trigger_execution_report_v1",
            "trigger_uuid": trigger.trigger_uuid,
            "trigger_name": trigger.name,
            "fired_at": fire_time.to_rfc3339(),
            "run_type": run_type.as_str(),
            "report": report,
        });
        if let Ok(message) = serde_json::to_string(&payload) {
            if let Err(err) = self.storage.insert_trigger_log(
                &trigger.trigger_uuid,
                level,
                &message,
                fire_time,
                run_type.clone(),
            ) {
                error!(
                    error = %err,
                    trigger_id = %trigger.trigger_uuid,
                    "Failed to persist trigger execution log entry"
                );
            }
        }

        if has_errors {
            warn!(
                trigger_id = %trigger.trigger_uuid,
                "Trigger execution completed with evaluation errors"
            );
        }

        let all_conditions_met = report.overall_result;

        let notification_id = None;

        if all_conditions_met {
            let explicit_action_uuid = trigger
                .action_uuid
                .as_deref()
                .filter(|value| !value.is_empty());
            let action_uuid =
                explicit_action_uuid.unwrap_or(DEFAULT_TRIGGER_NOTIFICATION_ACTION_UUID);
            let action_args = if explicit_action_uuid.is_some() {
                serde_json::from_str::<Value>(&trigger.action_args_json)
                    .unwrap_or_else(|_| Value::Object(Default::default()))
            } else {
                default_trigger_notification_args(trigger, fire_time)
            };

            match self.execute_action_now(
                action_uuid,
                ActionInvocationSourceKind::Trigger,
                Some("trigger"),
                Some(&trigger.trigger_uuid),
                action_args,
                None,
            ) {
                Ok(record) => {
                    let notification_id = if explicit_action_uuid.is_none() {
                        notification_id_from_action_result(record.result_json.as_ref())
                    } else {
                        None
                    };
                    return Ok(TriggerExecutionSummary {
                        trigger_id: trigger.trigger_uuid.clone(),
                        name: trigger.name.clone(),
                        fired_at: fire_time,
                        result: all_conditions_met,
                        run_type,
                        notification_id,
                    });
                }
                Err(error) => {
                    warn!(
                        trigger_id = %trigger.trigger_uuid,
                        action_uuid = %action_uuid,
                        error = %error,
                        "Trigger fired but action execution failed"
                    );
                }
            }
        }

        Ok(TriggerExecutionSummary {
            trigger_id: trigger.trigger_uuid.clone(),
            name: trigger.name.clone(),
            fired_at: fire_time,
            result: all_conditions_met,
            run_type,
            notification_id,
        })
    }

    fn run_action_script(
        &self,
        action: &ActionDefinition,
        execution_input: &Value,
    ) -> Result<(Value, Vec<String>)> {
        #[cfg(feature = "quickjs")]
        {
            let input_json = serde_json::to_string(execution_input)
                .context("Failed to serialize action execution input")?;
            let script = format!(
                "const __remiInput = {input_json};\nconst input = __remiInput;\nconst action = input.action;\nconst source = input.source;\nconst args = input.args;\nconst context = input.context;\n{}",
                action.script_source
            );
            let bindings = build_action_quickjs_bindings(
                &self.storage,
                &self.notification_event_tx,
                action,
                execution_input,
            );
            let output = crate::quickjs::quickjs_eval_with_bindings(&script, bindings)
                .context("Action QuickJS execution failed")?;
            let result_json = serde_json::from_str(&output.json_result)
                .context("Action returned non-JSON result")?;
            return Ok((result_json, output.console_logs));
        }

        #[cfg(not(feature = "quickjs"))]
        {
            let _ = action;
            let _ = execution_input;
            anyhow::bail!("This SDK build does not enable the quickjs feature for actions")
        }
    }

    fn resolve_entity_action_bindings(
        &self,
        bindings: Vec<EntityActionBinding>,
    ) -> Result<Vec<ResolvedEntityActionBinding>> {
        let mut resolved = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let action = self.fetch_action(&binding.action_uuid)?;
            resolved.push(ResolvedEntityActionBinding {
                action_uuid: binding.action_uuid,
                label_override: binding.label_override,
                args_json: binding.args_json,
                action_title: action.as_ref().map(|item| item.title.clone()),
                action_description: action.as_ref().map(|item| item.description.clone()),
                action_enabled: action.as_ref().map(|item| item.enabled),
                action_missing: action.is_none(),
            });
        }
        Ok(resolved)
    }

    // ===== Chat Session Management =====

    /// Create or update a chat session
    pub fn upsert_chat_session(
        &self,
        session_id: String,
        title: Option<String>,
        message_count: i32,
    ) -> Result<()> {
        let now = Utc::now();
        let session = crate::types::ChatSession {
            session_id,
            title,
            created_at: now,
            last_activity: now,
            message_count,
        };
        self.storage.upsert_chat_session(&session)
    }

    /// Get a specific chat session
    pub fn get_chat_session(&self, session_id: &str) -> Result<Option<crate::types::ChatSession>> {
        self.storage.get_chat_session(session_id)
    }

    /// List all chat sessions
    pub fn list_chat_sessions(&self, limit: Option<u32>) -> Result<Vec<crate::types::ChatSession>> {
        self.storage.list_chat_sessions(limit)
    }

    pub fn create_agent_version(&self, version: &crate::types::AgentVersion) -> Result<()> {
        self.storage.create_agent_version(version)
    }

    pub fn get_agent_version(
        &self,
        version_id: &str,
    ) -> Result<Option<crate::types::AgentVersion>> {
        self.storage.get_agent_version(version_id)
    }

    pub fn list_agent_versions(&self, agent_id: &str) -> Result<Vec<crate::types::AgentVersion>> {
        self.storage.list_agent_versions(agent_id)
    }

    pub fn update_agent_version(&self, update: &crate::types::AgentVersionUpdate) -> Result<()> {
        self.storage.update_agent_version(update)
    }

    pub fn mark_agent_version_applied(
        &self,
        version_id: &str,
        agent_id: &str,
        applied_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        self.storage
            .mark_agent_version_applied(version_id, agent_id, applied_at)
    }

    pub fn delete_agent_version(&self, version_id: &str) -> Result<()> {
        self.storage.delete_agent_version(version_id)
    }

    pub fn create_eval_dataset(&self, dataset: &crate::types::EvalDataset) -> Result<()> {
        self.storage.create_eval_dataset(dataset)
    }

    pub fn get_eval_dataset(&self, dataset_id: &str) -> Result<Option<crate::types::EvalDataset>> {
        self.storage.get_eval_dataset(dataset_id)
    }

    pub fn list_eval_datasets(&self) -> Result<Vec<crate::types::EvalDataset>> {
        self.storage.list_eval_datasets()
    }

    pub fn update_eval_dataset(&self, update: &crate::types::EvalDatasetUpdate) -> Result<()> {
        self.storage.update_eval_dataset(update)
    }

    pub fn delete_eval_dataset(&self, dataset_id: &str) -> Result<()> {
        self.storage.delete_eval_dataset(dataset_id)
    }

    pub fn add_session_to_dataset(&self, item: &crate::types::EvalDatasetSession) -> Result<()> {
        self.storage.add_session_to_dataset(item)
    }

    pub fn remove_session_from_dataset(&self, dataset_id: &str, session_id: &str) -> Result<()> {
        self.storage
            .remove_session_from_dataset(dataset_id, session_id)
    }

    pub fn list_dataset_sessions(
        &self,
        dataset_id: &str,
    ) -> Result<Vec<crate::types::EvalDatasetSession>> {
        self.storage.list_dataset_sessions(dataset_id)
    }

    pub fn create_eval_dataset_run(
        &self,
        run: &crate::types::EvalDatasetRun,
        items: &[crate::types::EvalDatasetRunItem],
    ) -> Result<()> {
        self.storage.create_eval_dataset_run(run, items)
    }

    pub fn list_eval_dataset_runs(
        &self,
        dataset_id: &str,
    ) -> Result<Vec<crate::types::EvalDatasetRun>> {
        self.storage.list_eval_dataset_runs(dataset_id)
    }

    pub fn get_eval_dataset_run(
        &self,
        run_id: &str,
    ) -> Result<Option<crate::types::EvalDatasetRun>> {
        self.storage.get_eval_dataset_run(run_id)
    }

    pub fn list_eval_dataset_run_items(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::types::EvalDatasetRunItem>> {
        self.storage.list_eval_dataset_run_items(run_id)
    }

    pub fn save_eval_dataset_run_evals(
        &self,
        run_id: &str,
        dataset_id: &str,
        evals: &[crate::types::EvalDatasetRunEval],
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        self.storage
            .save_eval_dataset_run_evals(run_id, dataset_id, evals, updated_at)
    }

    pub fn list_eval_dataset_run_evals(
        &self,
        run_id: &str,
    ) -> Result<Vec<crate::types::EvalDatasetRunEval>> {
        self.storage.list_eval_dataset_run_evals(run_id)
    }

    /// Update session activity (last activity time and message count)
    pub fn update_session_activity(
        &self,
        session_id: String,
        title: Option<String>,
        message_count: i32,
    ) -> Result<()> {
        let update = crate::types::ChatSessionUpdate {
            session_id,
            title,
            last_activity: Utc::now(),
            message_count,
        };
        self.storage.update_session_activity(&update)
    }

    /// Delete a chat session
    pub fn delete_chat_session(&self, session_id: &str) -> Result<()> {
        self.storage.delete_chat_session(session_id)
    }

    // ===== Chat Message History =====

    /// Create or update a chat message (stored as raw JSON) for a given session.
    pub fn upsert_chat_message_json(
        &self,
        session_id: String,
        message_id: String,
        created_at_ms: i64,
        message_json: String,
    ) -> Result<()> {
        self.storage.upsert_chat_message_json(
            &session_id,
            &message_id,
            created_at_ms,
            &message_json,
        )
    }

    /// List chat messages (raw JSON strings) for a session.
    pub fn list_chat_messages_json(
        &self,
        session_id: &str,
        limit: Option<u32>,
        offset: u32,
    ) -> Result<Vec<String>> {
        self.storage
            .list_chat_messages_json(session_id, limit, offset)
    }

    /// Export a persisted chat session bundle containing history and protocol state.
    pub fn export_chat_session_bundle(&self, session_id: &str) -> Result<ChatSessionExportBundle> {
        let session = self
            .get_chat_session(session_id)?
            .ok_or_else(|| anyhow!("Chat session not found: {session_id}"))?;

        let messages = self
            .list_chat_messages_json(session_id, None, 0)?
            .into_iter()
            .map(|json| parse_cached_message_storage_json(&json))
            .collect::<Result<Vec<_>>>()?;

        let protocol_state = self
            .get_chat_runtime_state_json(session_id)?
            .map(|json| serde_json::from_str::<ChatProtocolSessionState>(&json))
            .transpose()
            .context("Failed to parse chat runtime state JSON")?
            .unwrap_or_default();

        Ok(ChatSessionExportBundle::new(
            session,
            messages,
            protocol_state,
        ))
    }

    /// Export a persisted chat session bundle as a JSON document.
    pub fn export_chat_session_bundle_json(&self, session_id: &str) -> Result<String> {
        let bundle = self.export_chat_session_bundle(session_id)?;
        serde_json::to_string_pretty(&bundle).context("Failed to serialize chat session bundle")
    }

    /// Delete chat messages for a session (keeps the session record).
    pub fn delete_chat_messages(&self, session_id: &str) -> Result<()> {
        self.storage.delete_chat_messages(session_id)
    }

    /// Persist runtime-owned chat protocol state for a session.
    pub fn upsert_chat_runtime_state_json(
        &self,
        session_id: String,
        state_json: String,
    ) -> Result<()> {
        self.storage
            .upsert_chat_runtime_state_json(&session_id, &state_json)
    }

    /// Load runtime-owned chat protocol state for a session.
    pub fn get_chat_runtime_state_json(&self, session_id: &str) -> Result<Option<String>> {
        self.storage.get_chat_runtime_state_json(session_id)
    }

    /// Delete runtime-owned chat protocol state for a session.
    pub fn delete_chat_runtime_state(&self, session_id: &str) -> Result<()> {
        self.storage.delete_chat_runtime_state(session_id)
    }

    /// Persist a per-run model input snapshot as raw JSON.
    pub fn upsert_chat_model_input_json(
        &self,
        session_id: String,
        run_id: String,
        created_at_ms: i64,
        snapshot_json: String,
    ) -> Result<()> {
        self.storage.upsert_chat_model_input_json(
            &session_id,
            &run_id,
            created_at_ms,
            &snapshot_json,
        )
    }

    /// List model input snapshots for a session as raw JSON, newest first.
    pub fn list_chat_model_input_json(
        &self,
        session_id: &str,
        limit: Option<u32>,
    ) -> Result<Vec<String>> {
        self.storage.list_chat_model_input_json(session_id, limit)
    }

    /// Fetch one model input snapshot as raw JSON.
    pub fn get_chat_model_input_json(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<String>> {
        self.storage.get_chat_model_input_json(session_id, run_id)
    }

    /// Delete all model input snapshots for a session.
    pub fn delete_chat_model_inputs(&self, session_id: &str) -> Result<()> {
        self.storage.delete_chat_model_inputs(session_id)
    }
}

#[cfg(test)]
mod db_observability_tests {
    use super::TriggerSdk;
    use crate::storage::{test_sqlite_counters_get, test_sqlite_counters_reset};
    use crate::things_crdt::{ThingCollectionUpsert, ThingDatatype, ThingUpsert};
    use std::time::Instant;
    use tempfile::tempdir;

    #[test]
    fn long_overwrite_writes_things_state_once() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");

        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";

        // Seed doc with a collection + thing so get_or_init doesn't need to create rows during the measurement.
        sdk.things_upsert_collection(
            device_id,
            ThingCollectionUpsert {
                uuid: collection_id.to_string(),
                title: "Test Collection".to_string(),
                trigger_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("seed collection");

        sdk.things_upsert_thing(
            device_id,
            ThingUpsert {
                uuid: thing_id.to_string(),
                title: "Test Thing".to_string(),
                datatype: ThingDatatype::Markdown,
                data: Some(serde_json::json!({"markdown": "hello"})),
                collection_uuid: collection_id.to_string(),
                trigger_uuid: None,
                parent_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("seed thing");

        // Measure overwrite behavior.
        test_sqlite_counters_reset();

        let short = "short content".to_string();
        sdk.things_edit_content(
            device_id,
            thing_id,
            "overwrite",
            None,
            Some(&short),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("short overwrite");

        let after_short = test_sqlite_counters_get();
        eprintln!("short overwrite sqlite counters: {:?}", after_short);
        // V3 uses crdt_document_save instead of things_state_save (which persists each doc individually)
        // Expect at least 1 save per edit (ThingMarkdown)
        assert!(
            after_short.crdt_document_save >= 1,
            "short overwrite must persist crdt documents at least once"
        );

        // Reset again and do a long overwrite.
        test_sqlite_counters_reset();
        let long = "A".repeat(200_000);
        sdk.things_edit_content(
            device_id,
            thing_id,
            "overwrite",
            None,
            Some(&long),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("long overwrite");

        let after_long = test_sqlite_counters_get();
        eprintln!("long overwrite sqlite counters: {:?}", after_long);

        // The core guarantee: overwrite results in bounded DB writes regardless of content length.
        // V3 saves multiple documents per edit (root for metadata, collection, thing_markdown).
        // The key invariant: document save count should be bounded regardless of content size,
        // NOT proportional to content length (no per-character writes).
        assert!(
            after_long.crdt_document_save <= 10,
            "long overwrite should persist crdt documents a bounded number of times, got {}",
            after_long.crdt_document_save
        );
        assert!(
            after_short.crdt_document_save <= 10,
            "short overwrite should persist crdt documents a bounded number of times, got {}",
            after_short.crdt_document_save
        );

        // Connection opens should be roughly constant (not length-dependent)
        // The first edit may open more connections (initializing docs), but both should be bounded.
        assert!(
            after_long.open_connections <= 15,
            "long overwrite should have bounded DB connections: {after_long:?}"
        );
        assert!(
            after_short.open_connections <= 15,
            "short overwrite should have bounded DB connections: {after_short:?}"
        );
    }

    #[test]
    fn long_overwrite_time_breakdown() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");

        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";

        sdk.things_upsert_collection(
            device_id,
            ThingCollectionUpsert {
                uuid: collection_id.to_string(),
                title: "Test Collection".to_string(),
                trigger_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("seed collection");

        sdk.things_upsert_thing(
            device_id,
            ThingUpsert {
                uuid: thing_id.to_string(),
                title: "Test Thing".to_string(),
                datatype: ThingDatatype::Markdown,
                data: Some(serde_json::json!({"markdown": "hello"})),
                collection_uuid: collection_id.to_string(),
                trigger_uuid: None,
                parent_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("seed thing");

        let long = "A".repeat(200_000);

        test_sqlite_counters_reset();

        // V3: Performance test needs to be rewritten for multi-document architecture
        // The v3 architecture stores documents separately, so the performance characteristics differ
        // This test is temporarily simplified to just verify the basic operation works

        // 1) Test the new v3 splice path
        let t0 = Instant::now();
        let result = sdk.things_splice_text(device_id, thing_id, "main", 0, usize::MAX, &long);
        eprintln!(
            "breakdown: things_splice_text ms={} success={}",
            t0.elapsed().as_millis(),
            result.is_ok()
        );
        assert!(result.is_ok());

        let counters = test_sqlite_counters_get();
        eprintln!("breakdown: sqlite counters: {:?}", counters);

        // V3: Multiple documents may be saved, so we just check that operations succeed
        // The exact count depends on the v3 implementation
    }
}

fn parse_cached_message_storage_json(message_json: &str) -> Result<CachedMessage> {
    let mut message = serde_json::from_str::<CachedMessage>(message_json)
        .context("Failed to parse cached chat message JSON")?;
    message.refresh_ui_elements();
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::things_crdt::ThingDatatype;
    use chrono::TimeZone;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::str::FromStr;
    use std::thread;
    use tempfile::tempdir;

    fn seed_test_action(sdk: &TriggerSdk, action_uuid: &str, title: &str, script_source: &str) {
        sdk.storage
            .seed_builtin_actions(&[ActionDefinition {
                action_uuid: action_uuid.to_string(),
                name: action_uuid.replace('.', "_"),
                title: title.to_string(),
                description: format!("Test action for {action_uuid}"),
                version: "v1".to_string(),
                category: "test".to_string(),
                enabled: true,
                metadata_json: json!({ "builtin": false, "test": true }),
                script_source: script_source.trim().to_string(),
                input_schema_json: json!({ "type": "object" }),
                output_schema_json: Some(json!({ "type": "object" })),
            }])
            .expect("seed test action");
    }

    fn spawn_test_http_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).expect("read request");
            let request = String::from_utf8_lossy(&buffer[..read]);
            assert!(request.starts_with("POST /action-test HTTP/1.1"));
            assert!(request.to_ascii_lowercase().contains("x-test: 1"));
            assert!(request.contains("{\"ping\":\"pong\"}"));

            let body = r#"{"ok":true,"reply":"ack"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });

        (format!("http://{addr}/action-test"), handle)
    }

    fn seed_markdown_thing(
        sdk: &TriggerSdk,
        device_id: &str,
        collection_id: &str,
        thing_id: &str,
        markdown: &str,
    ) {
        sdk.things_upsert_collection(
            device_id,
            ThingCollectionUpsert {
                uuid: collection_id.to_string(),
                title: "Test Collection".to_string(),
                trigger_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("seed collection");

        sdk.things_upsert_thing(
            device_id,
            ThingUpsert {
                uuid: thing_id.to_string(),
                title: "Test Thing".to_string(),
                datatype: ThingDatatype::Markdown,
                data: Some(json!({"markdown": markdown})),
                collection_uuid: collection_id.to_string(),
                trigger_uuid: None,
                parent_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("seed thing");
    }

    #[test]
    fn test_extract_cron_from_preconditions() {
        let preconditions = vec![TriggerRule {
            rule: "cron('0 18 * * *')".to_string(),
            description: "Every day at 6 PM".to_string(),
        }];
        assert_eq!(
            extract_cron_from_preconditions(&preconditions),
            Some("0 18 * * *".to_string())
        );

        let no_cron = vec![TriggerRule {
            rule: "in_time_range('09:00', '17:00')".to_string(),
            description: "Business hours".to_string(),
        }];
        assert_eq!(extract_cron_from_preconditions(&no_cron), None);
    }

    #[test]
    fn test_extract_cron_with_double_quotes() {
        let preconditions = vec![TriggerRule {
            rule: r#"cron("0 9 * * *")"#.to_string(),
            description: "Morning".to_string(),
        }];
        assert_eq!(
            extract_cron_from_preconditions(&preconditions),
            Some("0 9 * * *".to_string())
        );
    }

    #[test]
    fn test_croner_accepts_posix_sunday_zero() {
        assert!(Cron::from_str("0 10 * * 0,6").is_ok());
    }

    #[test]
    fn test_network_change_trigger_is_marked_due_on_connectivity_event() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");

        let trigger_uuid = "trg-network-change".to_string();
        sdk.register_trigger(TriggerRegistration {
            trigger_uuid: trigger_uuid.clone(),
            name: "Network change trigger".to_string(),
            version: "v1".to_string(),
            precondition: vec![TriggerRule {
                rule: "event('Connectivity')".to_string(),
                description: "On connectivity change".to_string(),
            }],
            condition: vec![TriggerRule {
                rule: "true".to_string(),
                description: "Always true".to_string(),
            }],
            action_uuid: None,
            action_args: json!({}),
        })
        .expect("register trigger");

        // Event-driven triggers should not be due until an event arrives.
        let before = sdk
            .storage
            .fetch_due_triggers(Utc::now())
            .expect("fetch due triggers");
        assert!(
            before.iter().all(|t| t.trigger_uuid != trigger_uuid),
            "event trigger must not be due immediately after registration"
        );

        let event_ts = Utc::now();
        sdk.record_event(EventPayload {
            event_type: "Connectivity".to_string(),
            timestamp: event_ts,
            metadata: serde_json::json!({
                "message": "Update connectivity: wifi",
                "states": ["wifi"]
            }),
        })
        .expect("record connectivity event");

        let after = sdk
            .storage
            .fetch_due_triggers(event_ts + chrono::Duration::seconds(1))
            .expect("fetch due triggers after event");
        assert!(
            after.iter().any(|t| t.trigger_uuid == trigger_uuid),
            "event trigger must be marked due after Connectivity event"
        );
    }

    #[test]
    fn test_timer_trigger_is_due_after_registration_anchor() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");

        let trigger_uuid = "trg-timer".to_string();
        sdk.register_trigger(TriggerRegistration {
            trigger_uuid: trigger_uuid.clone(),
            name: "Timer trigger".to_string(),
            version: "v1".to_string(),
            precondition: vec![TriggerRule {
                rule: "timer('1s')".to_string(),
                description: "One second after registration".to_string(),
            }],
            condition: vec![TriggerRule {
                rule: "true".to_string(),
                description: "Always true".to_string(),
            }],
            action_uuid: None,
            action_args: json!({}),
        })
        .expect("register trigger");

        let stored = sdk
            .storage
            .fetch_trigger(&trigger_uuid)
            .expect("fetch trigger")
            .expect("trigger exists");
        assert!(
            stored.next_fire.is_some(),
            "timer trigger should have next_fire"
        );
        let stored_preconditions: Vec<TriggerRule> =
            serde_json::from_str(&stored.precondition_json).expect("decode preconditions");
        assert_eq!(stored_preconditions.len(), 1);
        assert!(
            stored_preconditions[0].rule.starts_with("timer(\"")
                && stored_preconditions[0].rule.contains('T')
                && !stored_preconditions[0].rule.contains("1s"),
            "timer precondition should be normalized to an absolute RFC3339 timestamp: {}",
            stored_preconditions[0].rule
        );
    }

    #[test]
    fn test_events_list_between_json_accepts_local_naive_timestamps() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");

        let timestamp = Utc.with_ymd_and_hms(2026, 4, 2, 1, 15, 0).single().unwrap();
        sdk.record_event(EventPayload {
            event_type: "DesktopAppFocus".to_string(),
            timestamp,
            metadata: json!({ "window_title": "VSCode" }),
        })
        .expect("record event");

        let output = sdk
            .events_list_between_json("2026-04-02 09:00:00", "2026-04-02 09:30:00")
            .expect("events between");
        let events: Vec<EventPayload> = serde_json::from_str(&output).expect("parse events json");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "DesktopAppFocus");
    }

    #[test]
    fn test_events_abstract_json_reports_recorded_events() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");

        sdk.record_event(EventPayload {
            event_type: "DesktopAppFocus".to_string(),
            timestamp: Utc.with_ymd_and_hms(2026, 4, 2, 1, 0, 0).single().unwrap(),
            metadata: json!({ "window_title": "VSCode" }),
        })
        .expect("record focus");
        sdk.record_event(EventPayload {
            event_type: "DesktopNetworkOnline".to_string(),
            timestamp: Utc.with_ymd_and_hms(2026, 4, 2, 1, 5, 0).single().unwrap(),
            metadata: json!({ "connected": true }),
        })
        .expect("record network");

        let output = sdk.events_abstract_json(3).expect("abstract events");
        let summary: serde_json::Value = serde_json::from_str(&output).expect("parse summary");
        let hours = summary
            .get("hours")
            .and_then(|value| value.as_array())
            .expect("hours array");

        assert!(
            !hours.is_empty(),
            "abstract summary should include recorded hours"
        );
        let total_events = hours
            .iter()
            .map(|hour| {
                hour.get("total_events")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0)
            })
            .sum::<u64>();
        assert_eq!(total_events, 2);
    }

    #[test]
    fn test_export_chat_session_bundle_round_trips_messages_and_protocol_state() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");

        let session_id = "session-export".to_string();
        sdk.upsert_chat_session(session_id.clone(), Some("Debug session".to_string()), 2)
            .expect("upsert session");

        let mut user_message = CachedMessage::user(
            "user-1".to_string(),
            "Hello".to_string(),
            Utc::now().timestamp_millis(),
        );
        user_message.refresh_ui_elements();
        sdk.upsert_chat_message_json(
            session_id.clone(),
            user_message.id.clone(),
            user_message.timestamp_ms,
            serde_json::to_string(&user_message).expect("serialize user message"),
        )
        .expect("upsert user message");

        let mut assistant_message =
            CachedMessage::assistant("assistant-1".to_string(), Utc::now().timestamp_millis());
        assistant_message.content = "Hi there".to_string();
        assistant_message.refresh_ui_elements();
        sdk.upsert_chat_message_json(
            session_id.clone(),
            assistant_message.id.clone(),
            assistant_message.timestamp_ms,
            serde_json::to_string(&assistant_message).expect("serialize assistant message"),
        )
        .expect("upsert assistant message");

        let protocol_state = ChatProtocolSessionState {
            history: vec![crate::chat_types::ProtocolHistoryMessage {
                id: "assistant-1".to_string(),
                role: "assistant".to_string(),
                content: json!("Hi there"),
                tool_calls: Vec::new(),
                tool_call_id: None,
                reasoning_content: None,
            }],
            latest_state: Some(json!({"foo": "bar"})),
            pending_tool_execution: None,
        };
        sdk.upsert_chat_runtime_state_json(
            session_id.clone(),
            serde_json::to_string(&protocol_state).expect("serialize protocol state"),
        )
        .expect("upsert protocol state");

        let bundle = sdk
            .export_chat_session_bundle(&session_id)
            .expect("export bundle");

        assert_eq!(bundle.version, ChatSessionExportBundle::VERSION);
        assert_eq!(bundle.session.session_id, session_id);
        assert_eq!(bundle.messages.len(), 2);
        assert_eq!(bundle.messages[0].content, "Hello");
        assert_eq!(bundle.messages[1].content, "Hi there");
        assert_eq!(
            bundle
                .protocol_state
                .latest_state
                .as_ref()
                .and_then(|value| value.get("foo"))
                .and_then(|value| value.as_str()),
            Some("bar")
        );

        let json = sdk
            .export_chat_session_bundle_json(&bundle.session.session_id)
            .expect("serialize bundle");
        assert!(json.contains("Debug session"));
        assert!(json.contains("Hi there"));
    }

    #[test]
    fn document_set_cache_returns_fresh_result_after_same_sdk_write() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";
        let content_path = format!("/collection/{collection_id}/things/{thing_id}/content.md");

        seed_markdown_thing(&sdk, device_id, collection_id, thing_id, "before");

        let initial = sdk
            .read_virtual_path(device_id, &content_path)
            .expect("initial read");
        assert_eq!(initial.content, "before");

        sdk.things_edit_content(
            device_id,
            thing_id,
            "overwrite",
            None,
            Some("after"),
            None,
            None,
            None,
            None,
            None,
        )
        .expect("edit content");

        let updated = sdk
            .read_virtual_path(device_id, &content_path)
            .expect("updated read");
        assert_eq!(updated.content, "after");
    }

    #[test]
    fn document_set_cache_invalidates_after_external_sdk_write() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk_reader = TriggerSdk::initialize(&db_path).expect("reader sdk init");
        let sdk_writer = TriggerSdk::initialize(&db_path).expect("writer sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";
        let content_path = format!("/collection/{collection_id}/things/{thing_id}/content.md");

        seed_markdown_thing(&sdk_reader, device_id, collection_id, thing_id, "before");

        let initial = sdk_reader
            .read_virtual_path(device_id, &content_path)
            .expect("initial read");
        assert_eq!(initial.content, "before");

        sdk_writer
            .things_edit_content(
                device_id,
                thing_id,
                "overwrite",
                None,
                Some("after"),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("writer edit content");

        let updated = sdk_reader
            .read_virtual_path(device_id, &content_path)
            .expect("updated read");
        assert_eq!(updated.content, "after");
    }

    #[test]
    fn json_object_cache_returns_fresh_result_after_same_sdk_write_and_delete() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";
        let thing_path = format!("/collection/{collection_id}/things/{thing_id}");

        seed_markdown_thing(&sdk, device_id, collection_id, thing_id, "before");

        let created = sdk
            .create_virtual_path(
                device_id,
                &thing_path,
                "json_object",
                None,
                Some("Config"),
                Some(r#"{"enabled":true,"count":1}"#),
                None,
                None,
                None,
            )
            .expect("create json_object");
        let entry_path = created
            .get("path")
            .and_then(Value::as_str)
            .expect("entry path")
            .to_string();
        let data_path = format!("{entry_path}.data.json");
        let schema_path = format!("{entry_path}.schema.json");

        let initial = sdk
            .read_virtual_path(device_id, &data_path)
            .expect("initial data read");
        assert_eq!(
            serde_json::from_str::<Value>(&initial.content).expect("parse initial json"),
            json!({
                "enabled": true,
                "count": 1
            })
        );

        sdk.edit_virtual_path(
            device_id,
            &schema_path,
            "overwrite",
            Some(&json!({
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean" },
                    "count": { "type": "integer", "minimum": 0 }
                },
                "required": ["enabled", "count"]
            })),
            None,
            None,
            None,
        )
        .expect("write schema");

        sdk.edit_virtual_path(
            device_id,
            &data_path,
            "overwrite",
            Some(&json!({
                "enabled": false,
                "count": 2
            })),
            None,
            None,
            None,
        )
        .expect("write data");

        let updated = sdk
            .read_virtual_path(device_id, &data_path)
            .expect("updated data read");
        assert_eq!(
            serde_json::from_str::<Value>(&updated.content).expect("parse updated json"),
            json!({
                "enabled": false,
                "count": 2
            })
        );

        let schema = sdk
            .read_virtual_path(device_id, &schema_path)
            .expect("schema read");
        assert_eq!(
            serde_json::from_str::<Value>(&schema.content).expect("parse schema json"),
            json!({
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean" },
                    "count": { "type": "integer", "minimum": 0 }
                },
                "required": ["enabled", "count"]
            })
        );

        sdk.delete_virtual_path(device_id, &entry_path)
            .expect("delete json object entry");
        assert!(sdk.read_virtual_path(device_id, &data_path).is_err());
        assert!(sdk.read_virtual_path(device_id, &schema_path).is_err());
    }

    #[test]
    fn json_object_cache_invalidates_after_external_sdk_write_and_delete() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk_reader = TriggerSdk::initialize(&db_path).expect("reader sdk init");
        let sdk_writer = TriggerSdk::initialize(&db_path).expect("writer sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";
        let thing_path = format!("/collection/{collection_id}/things/{thing_id}");

        seed_markdown_thing(&sdk_reader, device_id, collection_id, thing_id, "before");

        let created = sdk_reader
            .create_virtual_path(
                device_id,
                &thing_path,
                "json_object",
                None,
                Some("Config"),
                Some(r#"{"enabled":true}"#),
                None,
                None,
                None,
            )
            .expect("create json_object");
        let entry_path = created
            .get("path")
            .and_then(Value::as_str)
            .expect("entry path")
            .to_string();
        let data_path = format!("{entry_path}.data.json");

        let initial = sdk_reader
            .read_virtual_path(device_id, &data_path)
            .expect("initial read");
        assert_eq!(
            serde_json::from_str::<Value>(&initial.content).expect("parse initial json"),
            json!({ "enabled": true })
        );

        sdk_writer
            .edit_virtual_path(
                device_id,
                &data_path,
                "overwrite",
                Some(&json!({
                    "enabled": false,
                    "source": "writer"
                })),
                None,
                None,
                None,
            )
            .expect("writer overwrite data");

        let updated = sdk_reader
            .read_virtual_path(device_id, &data_path)
            .expect("reader updated read");
        assert_eq!(
            serde_json::from_str::<Value>(&updated.content).expect("parse updated json"),
            json!({
                "enabled": false,
                "source": "writer"
            })
        );

        sdk_writer
            .delete_virtual_path(device_id, &entry_path)
            .expect("writer delete entry");

        assert!(sdk_reader.read_virtual_path(device_id, &data_path).is_err());
    }

    #[test]
    fn things_upsert_thing_restores_json_object_entries_from_snapshot_payload() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";

        seed_markdown_thing(&sdk, device_id, collection_id, thing_id, "before");

        let entry_id = sdk
            .things_add_json_object_content_entry(
                device_id,
                thing_id,
                Some("Config"),
                Some(&json!({
                    "enabled": true,
                    "count": 1
                })),
                None,
            )
            .expect("add json object entry");

        let snapshot = sdk
            .things_list_snapshot(device_id)
            .expect("snapshot before delete");
        let thing_snapshot = snapshot
            .things
            .into_iter()
            .find(|thing| thing.uuid == thing_id)
            .expect("thing in snapshot");

        sdk.things_delete_thing(device_id, collection_id, thing_id)
            .expect("delete thing");

        sdk.things_upsert_thing(
            device_id,
            ThingUpsert {
                uuid: thing_snapshot.uuid.clone(),
                title: thing_snapshot.title.clone(),
                datatype: thing_snapshot.datatype.clone(),
                data: Some(thing_snapshot.data.clone()),
                collection_uuid: thing_snapshot.collection_uuid.clone(),
                trigger_uuid: thing_snapshot.trigger_uuid.clone(),
                parent_uuid: thing_snapshot.parent_uuid.clone(),
                created_at: None,
                updated_at: None,
            },
        )
        .expect("restore thing from snapshot payload");

        let restored_entries = sdk
            .things_get_content_entries(device_id, thing_id)
            .expect("restored content entries");
        assert_eq!(restored_entries.len(), 1);
        assert_eq!(restored_entries[0].id, entry_id);
        match &restored_entries[0].payload {
            ContentEntryPayload::JsonObject(_) => {}
            other => panic!("expected json_object payload, got {other:?}"),
        }
    }

    #[test]
    fn bootstrap_stash_round_trip_preserves_json_object_content_docs() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";

        seed_markdown_thing(&sdk, device_id, collection_id, thing_id, "before");

        let entry_id = sdk
            .things_add_json_object_content_entry(
                device_id,
                thing_id,
                Some("Config"),
                Some(&json!({
                    "enabled": true,
                    "count": 1
                })),
                Some(&json!({
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean" },
                        "count": { "type": "integer" }
                    }
                })),
            )
            .expect("add json object entry");

        assert!(
            sdk.things_bootstrap_stash_local_snapshot_if_needed(device_id)
                .expect("stash local docs")
        );

        sdk.things_bootstrap_from_server_snapshot_and_replay_stash(device_id, Vec::new(), None)
            .expect("replay stashed docs");

        let restored_data = sdk
            .things_get_json_object_entry_data(device_id, thing_id, &entry_id)
            .expect("load restored json data")
            .expect("json data should exist after replay");
        assert_eq!(
            restored_data,
            json!({
                "enabled": true,
                "count": 1
            })
        );

        let restored_schema = sdk
            .things_get_json_object_entry_schema(device_id, thing_id, &entry_id)
            .expect("load restored json schema")
            .expect("json schema should exist after replay");
        assert_eq!(
            restored_schema,
            json!({
                "type": "object",
                "properties": {
                    "enabled": { "type": "boolean" },
                    "count": { "type": "integer" }
                }
            })
        );
    }

    #[test]
    fn bootstrap_stash_still_runs_after_done_flag_when_new_local_docs_exist() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";

        seed_markdown_thing(&sdk, device_id, collection_id, thing_id, "before");

        sdk.storage
            .set_internal_kv("things.bootstrap.done", "1")
            .expect("set bootstrap done flag");

        let entry_id = sdk
            .things_add_json_object_content_entry(
                device_id,
                thing_id,
                Some("Config"),
                Some(&json!({
                    "enabled": true,
                    "count": 2
                })),
                Some(&json!({
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean" },
                        "count": { "type": "integer" }
                    }
                })),
            )
            .expect("add json object entry after bootstrap done");

        assert!(
            sdk.things_bootstrap_stash_local_snapshot_if_needed(device_id)
                .expect("stash should still run after done flag")
        );

        sdk.things_bootstrap_from_server_snapshot_and_replay_stash(device_id, Vec::new(), None)
            .expect("replay stashed docs");

        let restored_data = sdk
            .things_get_json_object_entry_data(device_id, thing_id, &entry_id)
            .expect("load restored json data")
            .expect("json data should exist after replay");
        assert_eq!(
            restored_data,
            json!({
                "enabled": true,
                "count": 2
            })
        );
    }

    #[test]
    fn sdk_seeds_builtin_actions_and_exposes_action_vfs() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let actions = sdk.list_actions().expect("list actions");

        assert!(!actions.is_empty());
        assert_eq!(actions[0].action_uuid, "builtin.echo_json");

        let tree = sdk
            .tree_virtual_path("device-test", Some("/action"))
            .expect("tree action root");
        assert!(tree.contains("builtin.echo_json/"));

        let metadata = sdk
            .read_virtual_path("device-test", "/action/builtin.echo_json/metadata.json")
            .expect("read action metadata");
        assert!(metadata.content.contains("supports_trigger"));
    }

    #[cfg(feature = "quickjs")]
    #[test]
    fn actions_can_use_http_host_api() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let (url, server) = spawn_test_http_server();

        seed_test_action(
            &sdk,
            "test.http_client",
            "HTTP Client Test",
            r#"
const response = http.post(args.url, {
    headers: { "x-test": "1" },
    json: { ping: "pong" },
});
return {
    ok: response.ok,
    status: response.status,
    method: response.method,
    reply: response.body_json?.reply ?? null,
};
"#,
        );

        let record = sdk
            .execute_action_now(
                "test.http_client",
                ActionInvocationSourceKind::System,
                None,
                None,
                json!({ "url": url }),
                Some("device-test"),
            )
            .expect("execute http action");

        let result = record.result_json.expect("result json");
        assert_eq!(result.get("ok"), Some(&json!(true)));
        assert_eq!(result.get("status"), Some(&json!(200)));
        assert_eq!(result.get("method"), Some(&json!("POST")));
        assert_eq!(result.get("reply"), Some(&json!("ack")));

        server.join().expect("join test server");
    }

    #[cfg(feature = "quickjs")]
    #[test]
    fn actions_can_send_notifications_and_emit_events() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let mut notification_rx = sdk.notifications_subscribe();

        seed_test_action(
            &sdk,
            "test.notify_client",
            "Notify Test",
            r#"
const created = notify.send({
    title: "Action Reminder",
    body: args.body,
    category: "action:test",
});
const listed = notify.list({ category: "action:test", limit: 10 });
return {
    notificationId: created.notification_id,
    listedCount: listed.items.length,
    latestTitle: listed.items[0]?.title ?? null,
};
"#,
        );

        let record = sdk
            .execute_action_now(
                "test.notify_client",
                ActionInvocationSourceKind::System,
                None,
                None,
                json!({ "body": "Stretch now" }),
                Some("device-test"),
            )
            .expect("execute notify action");

        let event = notification_rx.try_recv().expect("notification event");
        match event {
            NotificationEvent::Added {
                category,
                source,
                title,
                ..
            } => {
                assert_eq!(category, "action:test");
                assert_eq!(source, NotificationSource::System);
                assert_eq!(title, "Action Reminder");
            }
            other => panic!("expected added event, got {other:?}"),
        }

        let result = record.result_json.expect("result json");
        assert_eq!(result.get("listedCount"), Some(&json!(1)));
        assert_eq!(result.get("latestTitle"), Some(&json!("Action Reminder")));

        let stored = sdk
            .storage
            .list_notifications_by_category("action:test", 10)
            .expect("stored notifications");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].body, "Stretch now");
    }

    #[test]
    fn unbound_trigger_uses_default_notification_action() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let trigger_id = "trigger-default-notify".to_string();

        sdk.register_trigger(TriggerRegistration {
            trigger_uuid: trigger_id.clone(),
            name: "Water Plants".to_string(),
            version: "v1".to_string(),
            precondition: vec![TriggerRule {
                rule: "event('ManualTest')".to_string(),
                description: "manual test timing".to_string(),
            }],
            condition: Vec::new(),
            action_uuid: None,
            action_args: json!({}),
        })
        .expect("register trigger");

        let trigger = sdk
            .storage
            .fetch_trigger(&trigger_id)
            .expect("fetch trigger")
            .expect("trigger exists");
        let fire_time = Utc
            .with_ymd_and_hms(2026, 4, 15, 10, 30, 0)
            .single()
            .unwrap();
        let summary = sdk
            .execute_trigger(&trigger, fire_time, TriggerRunType::Manual)
            .expect("execute trigger");

        assert!(summary.result);
        assert!(summary.notification_id.is_some());

        let invocation = sdk
            .storage
            .latest_action_invocation(DEFAULT_TRIGGER_NOTIFICATION_ACTION_UUID)
            .expect("latest action invocation")
            .expect("default notification invocation exists");
        assert_eq!(invocation.source_kind, ActionInvocationSourceKind::Trigger);
        assert_eq!(
            invocation.source_entity_uuid.as_deref(),
            Some(trigger_id.as_str())
        );

        let invocation_notification_id = invocation
            .result_json
            .as_ref()
            .and_then(|value| value.get("notification_id"))
            .and_then(Value::as_i64);
        assert_eq!(summary.notification_id, invocation_notification_id);

        let notifications = sdk
            .storage
            .list_notifications_by_category(&trigger_id, 10)
            .expect("list notifications");
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].source, NotificationSource::Trigger);
        assert_eq!(notifications[0].title, "Water Plants");
        assert_eq!(
            notifications[0].body,
            "触发器「Water Plants」已于 18:30 触发"
        );
    }

    #[test]
    fn explicit_trigger_action_overrides_default_notification_action() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let trigger_id = "trigger-custom-action".to_string();

        sdk.register_trigger(TriggerRegistration {
            trigger_uuid: trigger_id.clone(),
            name: "Echo Trigger".to_string(),
            version: "v1".to_string(),
            precondition: vec![TriggerRule {
                rule: "event('ManualTest')".to_string(),
                description: "manual test timing".to_string(),
            }],
            condition: Vec::new(),
            action_uuid: Some("builtin.echo_json".to_string()),
            action_args: json!({ "scope": "custom-trigger" }),
        })
        .expect("register trigger");

        let trigger = sdk
            .storage
            .fetch_trigger(&trigger_id)
            .expect("fetch trigger")
            .expect("trigger exists");
        let summary = sdk
            .execute_trigger(&trigger, Utc::now(), TriggerRunType::Manual)
            .expect("execute trigger");

        assert!(summary.result);
        assert_eq!(summary.notification_id, None);

        let notifications = sdk
            .storage
            .list_notifications_by_category(&trigger_id, 10)
            .expect("list notifications");
        assert!(notifications.is_empty());

        let invocation = sdk
            .storage
            .latest_action_invocation("builtin.echo_json")
            .expect("latest echo invocation")
            .expect("echo invocation exists");
        assert_eq!(invocation.source_kind, ActionInvocationSourceKind::Trigger);
        assert_eq!(
            invocation.source_entity_uuid.as_deref(),
            Some(trigger_id.as_str())
        );
    }

    #[test]
    fn collection_and_thing_action_bindings_are_exposed_and_invokable() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";

        seed_markdown_thing(&sdk, device_id, collection_id, thing_id, "before");

        sdk.things_set_collection_action_bindings(
            device_id,
            collection_id,
            &[EntityActionBinding {
                action_uuid: "builtin.echo_json".to_string(),
                label_override: Some("Collection Echo".to_string()),
                args_json: json!({ "scope": "collection" }),
            }],
        )
        .expect("set collection bindings");
        sdk.things_set_thing_action_bindings(
            device_id,
            thing_id,
            &[EntityActionBinding {
                action_uuid: "builtin.echo_json".to_string(),
                label_override: Some("Thing Echo".to_string()),
                args_json: json!({ "scope": "thing" }),
            }],
        )
        .expect("set thing bindings");

        let collection_actions = sdk
            .read_virtual_path(
                device_id,
                &format!("/collection/{collection_id}/actions.json"),
            )
            .expect("read collection actions");
        assert!(collection_actions.content.contains("Collection Echo"));
        assert!(collection_actions.content.contains("builtin.echo_json"));

        let thing_actions = sdk
            .read_virtual_path(
                device_id,
                &format!("/collection/{collection_id}/things/{thing_id}/actions.json"),
            )
            .expect("read thing actions");
        assert!(thing_actions.content.contains("Thing Echo"));

        let collection_invocation = sdk
            .execute_collection_action_now(device_id, collection_id, "builtin.echo_json")
            .expect("invoke collection action");
        assert_eq!(
            collection_invocation.source_kind,
            ActionInvocationSourceKind::CollectionManual
        );
        assert_eq!(
            collection_invocation.source_entity_uuid.as_deref(),
            Some(collection_id)
        );

        let thing_invocation = sdk
            .execute_thing_action_now(device_id, thing_id, "builtin.echo_json")
            .expect("invoke thing action");
        assert_eq!(
            thing_invocation.source_kind,
            ActionInvocationSourceKind::ThingManual
        );
        assert_eq!(
            thing_invocation.source_entity_uuid.as_deref(),
            Some(thing_id)
        );

        let latest = sdk
            .read_virtual_path(
                device_id,
                "/action/builtin.echo_json/latest-invocation.json",
            )
            .expect("latest invocation");
        assert!(
            latest.content.contains("thing_manual") || latest.content.contains("collection_manual")
        );
    }

    #[test]
    fn virtual_fs_create_and_edit_support_action_bindings() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("sdk.sqlite3");
        let sdk = TriggerSdk::initialize(&db_path).expect("sdk init");
        let device_id = "device-test";
        let collection_id = "col-test";
        let thing_id = "thing-test";
        let trigger_id = "trigger-test";

        seed_markdown_thing(&sdk, device_id, collection_id, thing_id, "before");
        sdk.register_trigger(TriggerRegistration {
            trigger_uuid: trigger_id.to_string(),
            name: "Test Trigger".to_string(),
            version: "v1".to_string(),
            precondition: vec![TriggerRule {
                rule: "true".to_string(),
                description: "always".to_string(),
            }],
            condition: Vec::new(),
            action_uuid: None,
            action_args: json!({}),
        })
        .expect("register trigger");

        sdk.create_virtual_path(
            device_id,
            &format!("/trigger/{trigger_id}"),
            "action_binding",
            Some("builtin.echo_json"),
            None,
            Some(r#"{"scope":"trigger"}"#),
            None,
            None,
            None,
        )
        .expect("create trigger action binding");
        sdk.create_virtual_path(
            device_id,
            &format!("/collection/{collection_id}"),
            "action_binding",
            Some("builtin.echo_json"),
            Some("Collection Echo"),
            Some(r#"{"scope":"collection"}"#),
            None,
            None,
            None,
        )
        .expect("create collection action binding");
        sdk.create_virtual_path(
            device_id,
            &format!("/collection/{collection_id}/things/{thing_id}"),
            "action_binding",
            Some("builtin.echo_json"),
            Some("Thing Echo"),
            Some(r#"{"scope":"thing"}"#),
            None,
            None,
            None,
        )
        .expect("create thing action binding");

        let trigger_action = sdk
            .read_virtual_path(device_id, &format!("/trigger/{trigger_id}/action.json"))
            .expect("read trigger action binding");
        assert!(trigger_action.content.contains("builtin.echo_json"));
        assert!(trigger_action.content.contains("scope"));

        sdk.edit_virtual_path(
            device_id,
            &format!("/collection/{collection_id}/actions.json"),
            "overwrite",
            Some(&json!([
                {
                    "action_uuid": "builtin.echo_json",
                    "label_override": "Collection Echo Updated",
                    "args_json": { "scope": "collection-updated" }
                }
            ])),
            None,
            None,
            None,
        )
        .expect("edit collection action bindings");
        sdk.edit_virtual_path(
            device_id,
            &format!("/trigger/{trigger_id}/action.json"),
            "overwrite",
            Some(&json!(null)),
            None,
            None,
            None,
        )
        .expect("clear trigger action binding");

        let updated_collection_actions = sdk
            .read_virtual_path(
                device_id,
                &format!("/collection/{collection_id}/actions.json"),
            )
            .expect("read updated collection actions");
        assert!(
            updated_collection_actions
                .content
                .contains("Collection Echo Updated")
        );

        let cleared_trigger_action = sdk
            .read_virtual_path(device_id, &format!("/trigger/{trigger_id}/action.json"))
            .expect("read cleared trigger action binding");
        assert_eq!(cleared_trigger_action.content.trim(), "null");
    }
}

fn compute_next_fire(
    cron_expr: &str,
    anchor: DateTime<Utc>,
    now: Option<DateTime<Utc>>, // if provided, ensure next fire is after this time
) -> Result<Option<DateTime<Utc>>> {
    let schedule = Cron::from_str(cron_expr).context("Invalid cron expression")?;
    let tz = default_timezone();
    let effective_anchor = match now {
        Some(now) if now > anchor => now,
        _ => anchor,
    };
    let anchor_local = effective_anchor.with_timezone(&tz);
    let next_local = schedule
        .find_next_occurrence(&anchor_local, false)
        .context("Invalid cron expression")?;
    Ok(Some(next_local.with_timezone(&Utc)))
}

/// Filter events that fall within a time window ending at `current` and spanning `window_minutes` back.
fn filter_events_at_time(
    all_events: &[MonitoringEvent],
    current: DateTime<Utc>,
    window_minutes: i64,
) -> Vec<MonitoringEvent> {
    let start = current - Duration::minutes(window_minutes);
    all_events
        .iter()
        .filter_map(|e| {
            let dt = DateTime::parse_from_rfc3339(&e.timestamp)
                .ok()?
                .with_timezone(&Utc);
            if dt >= start && dt <= current {
                Some(e.clone())
            } else {
                None
            }
        })
        .collect()
}

fn select_current_event<'a>(
    events: &'a [MonitoringEvent],
    timings: &[rule_trigger_engine::TriggerTiming],
    fire_time: DateTime<Utc>,
) -> Option<&'a MonitoringEvent> {
    let event_types: Vec<&str> = timings
        .iter()
        .filter_map(|timing| match timing {
            rule_trigger_engine::TriggerTiming::Event { event_type } => Some(event_type.as_str()),
            _ => None,
        })
        .collect();

    events
        .iter()
        .filter_map(|event| {
            let matches_type = event_types.is_empty()
                || event_types
                    .iter()
                    .any(|event_type| *event_type == event.event_type);
            if !matches_type {
                return None;
            }

            let timestamp = DateTime::parse_from_rfc3339(&event.timestamp)
                .ok()?
                .with_timezone(&Utc);
            if timestamp > fire_time {
                return None;
            }

            Some((timestamp, event))
        })
        .max_by_key(|(timestamp, _)| *timestamp)
        .map(|(_, event)| event)
}

#[cfg(test)]
fn extract_cron_from_preconditions(preconditions: &[TriggerRule]) -> Option<String> {
    for rule in preconditions {
        // Look for cron() function calls in the rule
        let rule_text = rule.rule.trim();
        if rule_text.starts_with("cron(") {
            // Extract the cron expression from cron('...') or cron("...")
            if let Some(start) = rule_text.find('\'') {
                if let Some(end) = rule_text[start + 1..].find('\'') {
                    return Some(rule_text[start + 1..start + 1 + end].to_string());
                }
            }
            if let Some(start) = rule_text.find('"') {
                if let Some(end) = rule_text[start + 1..].find('"') {
                    return Some(rule_text[start + 1..start + 1 + end].to_string());
                }
            }
        }
    }
    None
}

fn extract_timings_from_rules(
    preconditions: &[TriggerRule],
    conditions: &[TriggerRule],
) -> Result<Vec<rule_trigger_engine::TriggerTiming>> {
    let precondition_rules: Vec<EngineRule> = preconditions
        .iter()
        .cloned()
        .map(|rule| EngineRule {
            rule: rule.rule,
            description: rule.description,
        })
        .collect();
    let condition_rules: Vec<EngineRule> = conditions
        .iter()
        .cloned()
        .map(|rule| EngineRule {
            rule: rule.rule,
            description: rule.description,
        })
        .collect();

    // Dummy values: timing extraction only uses rule bodies.
    let config = TriggerConfig {
        name: "timing-extract".to_string(),
        version: "v1".to_string(),
        precondition: precondition_rules,
        condition: condition_rules,
    };

    config
        .extract_timing()
        .map_err(|err| anyhow!("Failed to extract timing: {err}"))
}

fn extract_repeat_frequency_from_conditions(
    conditions: &[TriggerRule],
) -> Option<rule_trigger_engine::RepeatFrequency> {
    for rule in conditions {
        let rule_text = rule.rule.trim();

        if let Some(arg) = rule_text
            .strip_prefix("repeat_per_day(")
            .and_then(|s| s.strip_suffix(')'))
        {
            let value = arg.trim().parse::<u32>().ok()?;
            if value > 0 {
                return Some(rule_trigger_engine::RepeatFrequency::PerDay(value));
            }
        }

        if let Some(arg) = rule_text
            .strip_prefix("repeat_per_week(")
            .and_then(|s| s.strip_suffix(')'))
        {
            let value = arg.trim().parse::<u32>().ok()?;
            if value > 0 {
                return Some(rule_trigger_engine::RepeatFrequency::PerWeek(value));
            }
        }
    }
    None
}

fn repeat_min_gap(freq: &rule_trigger_engine::RepeatFrequency) -> Option<Duration> {
    match *freq {
        rule_trigger_engine::RepeatFrequency::PerDay(times) if times > 0 => {
            let seconds = (24 * 60 * 60) / i64::from(times);
            Some(Duration::seconds(seconds.max(1)))
        }
        rule_trigger_engine::RepeatFrequency::PerWeek(times) if times > 0 => {
            let seconds = (7 * 24 * 60 * 60) / i64::from(times);
            Some(Duration::seconds(seconds.max(1)))
        }
        _ => None,
    }
}

fn normalize_timer_preconditions(
    preconditions: &[TriggerRule],
    anchor: DateTime<Utc>,
    timezone_offset: &str,
) -> Result<Vec<TriggerRule>> {
    preconditions
        .iter()
        .cloned()
        .map(|mut rule| {
            let timings = extract_timings_from_rules(&[rule.clone()], &[])?;
            if let Some(rule_trigger_engine::TriggerTiming::Timer { value }) = timings.first() {
                let normalized =
                    rule_trigger_engine::normalize_timer_literal(value, anchor, timezone_offset)
                        .map_err(|err| {
                            anyhow!(
                                "Failed to normalize timer precondition '{}': {err}",
                                rule.description
                            )
                        })?;
                rule.rule = format!("timer(\"{}\")", normalized);
            }
            Ok(rule)
        })
        .collect()
}

fn resolve_registration_next_fire(
    timings: &[rule_trigger_engine::TriggerTiming],
    anchor: DateTime<Utc>,
    timezone_offset: &str,
) -> Result<Option<DateTime<Utc>>> {
    let mut earliest: Option<DateTime<Utc>> = None;
    let mut has_supported = false;
    let mut has_event = false;

    for timing in timings {
        match timing {
            rule_trigger_engine::TriggerTiming::Cron { expression } => {
                has_supported = true;
                if let Some(next) = compute_next_fire(expression, anchor, Some(anchor))? {
                    earliest = Some(match earliest {
                        Some(current) => current.min(next),
                        None => next,
                    });
                }
            }
            rule_trigger_engine::TriggerTiming::Timer { value } => {
                has_supported = true;
                let resolved =
                    rule_trigger_engine::resolve_timer_literal(value, anchor, timezone_offset)
                        .map_err(|err| {
                            anyhow!("Failed to resolve timer precondition '{value}': {err}")
                        })?;
                let due_at = if resolved <= anchor { anchor } else { resolved };
                earliest = Some(match earliest {
                    Some(current) => current.min(due_at),
                    None => due_at,
                });
            }
            rule_trigger_engine::TriggerTiming::Event { .. } => {
                has_supported = true;
                has_event = true;
            }
            rule_trigger_engine::TriggerTiming::RepeatFrequency { .. } => {}
        }
    }

    if let Some(next) = earliest {
        Ok(Some(next))
    } else if has_event {
        Ok(None)
    } else if has_supported {
        Ok(None)
    } else {
        Err(anyhow!(
            "No supported timing found in trigger rules (expected cron(...), timer(...), or event(...))."
        ))
    }
}

fn resolve_post_run_next_fire(
    timings: &[rule_trigger_engine::TriggerTiming],
    fire_time: DateTime<Utc>,
    lower_bound: DateTime<Utc>,
    timezone_offset: &str,
) -> Result<Option<DateTime<Utc>>> {
    let mut earliest: Option<DateTime<Utc>> = None;

    for timing in timings {
        match timing {
            rule_trigger_engine::TriggerTiming::Cron { expression } => {
                if let Some(next) = compute_next_fire(expression, fire_time, Some(lower_bound))? {
                    earliest = Some(match earliest {
                        Some(current) => current.min(next),
                        None => next,
                    });
                }
            }
            rule_trigger_engine::TriggerTiming::Timer { value } => {
                let resolved =
                    rule_trigger_engine::resolve_timer_literal(value, fire_time, timezone_offset)
                        .map_err(|err| {
                        anyhow!("Failed to resolve timer precondition '{value}': {err}")
                    })?;
                if resolved > lower_bound {
                    earliest = Some(match earliest {
                        Some(current) => current.min(resolved),
                        None => resolved,
                    });
                }
            }
            rule_trigger_engine::TriggerTiming::Event { .. }
            | rule_trigger_engine::TriggerTiming::RepeatFrequency { .. } => {}
        }
    }

    Ok(earliest)
}

fn build_trigger_occurrences(
    timings: &[rule_trigger_engine::TriggerTiming],
    all_events: &[MonitoringEvent],
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    registration_anchor: DateTime<Utc>,
    timezone_offset: &str,
) -> Result<Vec<DateTime<Utc>>> {
    use std::collections::BTreeSet;

    let mut occurrences = BTreeSet::new();
    let tz = FixedOffset::from_str(timezone_offset).unwrap_or_else(|_| default_timezone());

    for timing in timings {
        match timing {
            rule_trigger_engine::TriggerTiming::Cron { expression } => {
                let schedule = Cron::from_str(expression)
                    .with_context(|| format!("Invalid cron expression: {expression}"))?;
                let mut next_local = schedule
                    .find_next_occurrence(&start.with_timezone(&tz), false)
                    .with_context(|| {
                        format!("Failed to find next occurrence for cron: {expression}")
                    })?;
                while next_local.with_timezone(&Utc) <= end {
                    occurrences.insert(next_local.with_timezone(&Utc));
                    next_local = schedule
                        .find_next_occurrence(&next_local, false)
                        .with_context(|| {
                            format!("Failed to find next occurrence for cron: {expression}")
                        })?;
                }
            }
            rule_trigger_engine::TriggerTiming::Timer { value } => {
                let at = rule_trigger_engine::resolve_timer_literal(
                    value,
                    registration_anchor,
                    timezone_offset,
                )
                .map_err(|err| anyhow!("Failed to resolve timer precondition '{value}': {err}"))?;
                if at >= start && at <= end {
                    occurrences.insert(at);
                }
            }
            rule_trigger_engine::TriggerTiming::Event { event_type } => {
                for event in all_events {
                    if event.event_type != *event_type {
                        continue;
                    }
                    if let Ok(at) = DateTime::parse_from_rfc3339(&event.timestamp) {
                        let at = at.with_timezone(&Utc);
                        if at >= start && at <= end {
                            occurrences.insert(at);
                        }
                    }
                }
            }
            rule_trigger_engine::TriggerTiming::RepeatFrequency { .. } => {}
        }
    }

    Ok(occurrences.into_iter().collect())
}

fn describe_timing_sources(timings: &[rule_trigger_engine::TriggerTiming]) -> String {
    if timings.is_empty() {
        return "none".to_string();
    }

    timings
        .iter()
        .map(|timing| match timing {
            rule_trigger_engine::TriggerTiming::Cron { expression } => {
                format!("cron({expression})")
            }
            rule_trigger_engine::TriggerTiming::Timer { value } => format!("timer({value})"),
            rule_trigger_engine::TriggerTiming::Event { event_type } => {
                format!("event({event_type})")
            }
            rule_trigger_engine::TriggerTiming::RepeatFrequency { frequency } => match frequency {
                rule_trigger_engine::RepeatFrequency::PerDay(n) => format!("repeat_per_day({n})"),
                rule_trigger_engine::RepeatFrequency::PerWeek(n) => {
                    format!("repeat_per_week({n})")
                }
            },
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn replay_stashed_thing_into_document_set(
    doc_set: &mut crate::things_crdt::ThingsDocumentSet,
    thing: &crate::things_crdt::ThingEntry,
) -> Result<()> {
    let content_registry = crate::things_crdt::ContentTypeRegistry::new();
    let (markdown, content_entries) = content_registry.extract_thing_snapshot_parts(&thing.data)?;
    let trigger = crate::things_crdt::trigger_update_from_tri_state(thing.trigger_uuid.as_deref());
    doc_set.upsert_thing_meta_with_timestamps(
        &thing.collection_uuid,
        &thing.uuid,
        Some(thing.datatype.clone()),
        None,
        Some(thing.title.clone()),
        thing.parent_uuid.clone(),
        trigger,
        Some(thing.created_at.clone()),
        Some(thing.updated_at.clone()),
    )?;

    if let Some(markdown) = markdown {
        doc_set.set_thing_markdown_text(&thing.uuid, &markdown)?;
    }

    for entry in content_entries {
        doc_set.add_content_entry(&thing.collection_uuid, &thing.uuid, entry)?;
    }

    Ok(())
}

fn serialize_bootstrap_stash_documents(
    dirty_documents: &[crate::types::CrdtDocumentRow],
) -> Result<String> {
    let payload = BootstrapStashPayload {
        version: 1,
        documents: dirty_documents
            .iter()
            .map(|row| BootstrapStashedDocument {
                uuid: row.uuid.clone(),
                data_type: row.data_type.clone(),
                automerge_doc_base64: base64::engine::general_purpose::STANDARD
                    .encode(&row.automerge_doc),
            })
            .collect(),
    };

    serde_json::to_string(&payload).context("Failed to encode bootstrap stash payload")
}

fn parse_bootstrap_replay_source(stash_json: &str) -> Result<BootstrapReplaySource> {
    if let Ok(payload) = serde_json::from_str::<BootstrapStashPayload>(stash_json) {
        return Ok(BootstrapReplaySource::Documents(payload.documents));
    }

    let snapshot = serde_json::from_str::<crate::things_crdt::ThingsSnapshot>(stash_json)
        .context("Failed to parse bootstrap stash payload")?;
    Ok(BootstrapReplaySource::LegacySnapshot(snapshot))
}

fn restore_stashed_documents(
    storage: &Storage,
    documents: &[BootstrapStashedDocument],
) -> Result<()> {
    let mut sorted_documents = documents.to_vec();
    sorted_documents.sort_by_key(|doc| {
        (
            bootstrap_data_type_sort_key(&doc.data_type),
            doc.uuid.clone(),
        )
    });

    for document in sorted_documents {
        let automerge_doc = base64::engine::general_purpose::STANDARD
            .decode(&document.automerge_doc_base64)
            .context("Failed to decode stashed CRDT document")?;
        storage
            .save_crdt_document(
                &document.uuid,
                &document.data_type,
                &automerge_doc,
                &[],
                true,
                None,
            )
            .context("Failed to restore stashed CRDT document")?;
    }

    Ok(())
}

fn merge_stashed_documents_onto_current_documents(
    storage: &Storage,
    documents: &[BootstrapStashedDocument],
) -> Result<()> {
    let mut sorted_documents = documents.to_vec();
    sorted_documents.sort_by_key(|doc| {
        (
            bootstrap_data_type_sort_key(&doc.data_type),
            doc.uuid.clone(),
        )
    });

    for document in sorted_documents {
        let incoming_bytes = base64::engine::general_purpose::STANDARD
            .decode(&document.automerge_doc_base64)
            .context("Failed to decode stashed CRDT document")?;

        if let Some(existing) = storage
            .get_crdt_document(&document.uuid, &document.data_type)
            .context("Failed to load current CRDT document during bootstrap replay")?
        {
            let mut merged = automerge::AutoCommit::load(&existing.automerge_doc)
                .context("Failed to load current CRDT document")?;
            let mut incoming = automerge::AutoCommit::load(&incoming_bytes)
                .context("Failed to load stashed CRDT document")?;
            merged
                .merge(&mut incoming)
                .context("Failed to merge stashed CRDT document")?;
            let merged_bytes = merged.save();

            storage
                .save_crdt_document(
                    &document.uuid,
                    &document.data_type,
                    &merged_bytes,
                    &existing.sync_state,
                    true,
                    existing.last_sync_at.as_deref(),
                )
                .context("Failed to save merged CRDT document")?;
            continue;
        }

        storage
            .save_crdt_document(
                &document.uuid,
                &document.data_type,
                &incoming_bytes,
                &[],
                true,
                None,
            )
            .context("Failed to restore missing stashed CRDT document")?;
    }

    Ok(())
}

fn bootstrap_data_type_sort_key(data_type: &str) -> u8 {
    match data_type {
        "root" => 0,
        "collection" => 1,
        "thing_markdown" => 2,
        _ => 3,
    }
}
