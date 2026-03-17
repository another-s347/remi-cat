use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use remi_agentloop::prelude::{AgentError, ToolContext};
use remi_client_sdk::auth::auth_set_app_key;
use remi_client_sdk::things_crdt::{ThingCollectionUpsert, ThingUpsert, ThingsSnapshot};
use remi_client_sdk::things_sync::sync_v3_documents_with_server;
use remi_client_sdk::transport::configure_shared_transport;
use remi_client_sdk::{TriggerClient, TriggerRegistration, TriggerRule, TriggerSdk};
use remi_things_crdt::{apply_collection_op, CollectionOp, ThingDatatype, TriggerUpdate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{Mutex, OnceCell};
use tracing::warn;
use uuid::Uuid;

use super::tools::{
    triggers_from_user_state, write_triggers_to_user_state, TriggerItem, TriggerRuleSpec,
    TriggerUpsertRequest, TriggerUpsertResult,
};

const REMI_APP_KEY_ENV: &str = "REMI_APP_KEY";
const REMI_PUBLIC_GRPC_ADDR_ENV: &str = "REMI_PUBLIC_GRPC_ADDR";
const REMI_TRIGGER_DEVICE_ID_ENV: &str = "REMI_TRIGGER_DEVICE_ID";
const TRIGGER_TOOLS_ENABLED_META_KEY: &str = "trigger_tools_enabled";
const SDK_ATTRS_ROOT_KEY: &str = "remi_cat_trigger";
const SDK_ATTRS_SOURCE: &str = "remi-cat-trigger";
const SDK_DB_FILE_NAME: &str = "trigger-sdk.db";
const TRIGGER_COLLECTION_TITLE: &str = "Semantic triggers";

pub struct TriggerBackend {
    sdk_config: Option<SdkTriggerConfig>,
    sdk_runtime: OnceCell<Arc<SdkTriggerRuntime>>,
}

impl TriggerBackend {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            sdk_config: SdkTriggerConfig::from_env(&data_dir),
            sdk_runtime: OnceCell::new(),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.sdk_config.is_some()
    }

    pub async fn refresh_thread_user_state(
        &self,
        thread_id: &str,
        user_state: &mut Value,
    ) -> Result<()> {
        let Some(runtime) = self.sdk_runtime().await? else {
            return Ok(());
        };

        runtime
            .refresh_thread_user_state(thread_id, user_state)
            .await
    }

    pub(crate) async fn upsert(
        &self,
        ctx: &ToolContext,
        request: TriggerUpsertRequest,
    ) -> Result<TriggerUpsertResult, AgentError> {
        ensure_trigger_tools_allowed(ctx)?;

        let thread_id = thread_id_from_ctx(ctx).ok_or_else(|| {
            AgentError::tool("trigger__upsert", "missing thread_id in tool context")
        })?;
        let mut user_state = self.load_user_state(ctx).await;
        let mut triggers = triggers_from_user_state(&user_state);
        let existing = request
            .id
            .and_then(|id| triggers.iter().find(|item| item.id == id).cloned());
        if request.id.is_some() && existing.is_none() {
            store_user_state(ctx, user_state);
            return Err(AgentError::tool(
                "trigger__upsert",
                format!("trigger #{} not found", request.id.unwrap_or_default()),
            ));
        }

        let local_id = existing
            .as_ref()
            .map(|item| item.id)
            .unwrap_or_else(|| next_local_id(&triggers));
        let draft = TriggerDraft {
            id: local_id,
            trigger_uuid: existing
                .as_ref()
                .map(|item| item.trigger_uuid.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            thing_uuid: existing
                .as_ref()
                .map(|item| item.thing_uuid.clone())
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            collection_uuid: existing
                .as_ref()
                .map(|item| item.collection_uuid.clone())
                .unwrap_or_else(|| collection_uuid_for(thread_id)),
            thread_id: thread_id.to_string(),
            name: request.name,
            request: request.request,
            precondition: request.precondition,
            condition: request.condition,
            enabled: request.enabled,
            owner_user_id: metadata_string(ctx, "sender_user_id").or_else(|| {
                existing
                    .as_ref()
                    .and_then(|item| item.owner_user_id.clone())
            }),
            owner_username: metadata_string(ctx, "sender_username").or_else(|| {
                existing
                    .as_ref()
                    .and_then(|item| item.owner_username.clone())
            }),
            reply_to_message_id: metadata_string(ctx, "message_id"),
            chat_type: metadata_string(ctx, "chat_type"),
            platform: metadata_string(ctx, "platform"),
        };

        let runtime = self.require_sdk("trigger__upsert").await?;
        let item = runtime.upsert_trigger(draft).await.map_err(|err| {
            AgentError::tool(
                "trigger__upsert",
                format!("failed to persist trigger: {err:#}"),
            )
        })?;

        match triggers.iter().position(|existing| existing.id == item.id) {
            Some(index) => triggers[index] = item.clone(),
            None => triggers.push(item.clone()),
        }
        triggers.sort_by_key(|item| item.id);
        write_triggers_to_user_state(&mut user_state, &triggers);
        store_user_state(ctx, user_state);

        Ok(TriggerUpsertResult {
            operation: if existing.is_some() {
                "updated".to_string()
            } else {
                "created".to_string()
            },
            item,
        })
    }

    pub(crate) async fn list(&self, ctx: &ToolContext) -> Result<Vec<TriggerItem>, AgentError> {
        ensure_trigger_tools_allowed(ctx)?;

        let user_state = self.load_user_state(ctx).await;
        let triggers = triggers_from_user_state(&user_state);
        store_user_state(ctx, user_state);
        Ok(triggers)
    }

    pub(crate) async fn delete(&self, ctx: &ToolContext, id: u64) -> Result<String, AgentError> {
        ensure_trigger_tools_allowed(ctx)?;

        let mut user_state = self.load_user_state(ctx).await;
        let mut triggers = triggers_from_user_state(&user_state);
        let Some(index) = triggers.iter().position(|item| item.id == id) else {
            store_user_state(ctx, user_state);
            return Ok(format!("Trigger #{id} not found."));
        };

        let item = triggers[index].clone();
        let runtime = self.require_sdk("trigger__delete").await?;
        runtime.delete_trigger(&item).await.map_err(|err| {
            AgentError::tool(
                "trigger__delete",
                format!("failed to delete trigger: {err:#}"),
            )
        })?;
        triggers.remove(index);
        write_triggers_to_user_state(&mut user_state, &triggers);
        store_user_state(ctx, user_state);

        Ok(format!("Removed trigger #{id}."))
    }

    async fn require_sdk(&self, tool_name: &str) -> Result<Arc<SdkTriggerRuntime>, AgentError> {
        match self.sdk_runtime().await {
            Ok(Some(runtime)) => Ok(runtime),
            Ok(None) => Err(AgentError::tool(
                tool_name,
                format!(
                    "remi trigger backend is not configured; set {REMI_APP_KEY_ENV} and {REMI_PUBLIC_GRPC_ADDR_ENV}"
                ),
            )),
            Err(err) => Err(AgentError::tool(
                tool_name,
                format!("remi trigger backend is unavailable: {err:#}"),
            )),
        }
    }

    async fn sdk_runtime(&self) -> Result<Option<Arc<SdkTriggerRuntime>>> {
        let Some(config) = self.sdk_config.clone() else {
            return Ok(None);
        };

        let runtime = self
            .sdk_runtime
            .get_or_try_init(
                || async move { SdkTriggerRuntime::initialize(config).await.map(Arc::new) },
            )
            .await?;
        Ok(Some(runtime.clone()))
    }

    async fn load_user_state(&self, ctx: &ToolContext) -> Value {
        let mut user_state = { ctx.user_state.read().unwrap().clone() };
        if let Some(thread_id) = thread_id_from_ctx(ctx) {
            if let Err(err) = self
                .refresh_thread_user_state(thread_id, &mut user_state)
                .await
            {
                warn!(
                    thread_id,
                    error = %err,
                    "failed to refresh sdk-backed trigger state before tool execution"
                );
            }
        }
        user_state
    }
}

#[derive(Debug, Clone)]
struct SdkTriggerConfig {
    app_key: String,
    public_grpc_addr: String,
    device_id: String,
    db_path: PathBuf,
}

impl SdkTriggerConfig {
    fn from_env(data_dir: &Path) -> Option<Self> {
        let app_key = std::env::var(REMI_APP_KEY_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let public_grpc_addr = std::env::var(REMI_PUBLIC_GRPC_ADDR_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        match (app_key, public_grpc_addr) {
            (Some(app_key), Some(public_grpc_addr)) => Some(Self {
                app_key,
                public_grpc_addr,
                device_id: std::env::var(REMI_TRIGGER_DEVICE_ID_ENV)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| default_device_id(data_dir)),
                db_path: data_dir.join(SDK_DB_FILE_NAME),
            }),
            (None, None) => None,
            _ => {
                warn!(
                    "partial remi trigger configuration detected; trigger backend disabled until both {} and {} are set",
                    REMI_APP_KEY_ENV,
                    REMI_PUBLIC_GRPC_ADDR_ENV,
                );
                None
            }
        }
    }
}

struct SdkTriggerRuntime {
    config: SdkTriggerConfig,
    sdk: Arc<TriggerSdk>,
    lock: Arc<Mutex<()>>,
}

impl SdkTriggerRuntime {
    async fn initialize(config: SdkTriggerConfig) -> Result<Self> {
        let transport_config = serde_json::json!({
            "transportMode": "tcp",
            "tcpGrpcAddr": config.public_grpc_addr,
            "connectTimeoutMs": 3000,
            "requestTimeoutMs": 10000,
        })
        .to_string();

        configure_shared_transport(&transport_config)
            .await
            .map_err(|err| anyhow::anyhow!(err))?;
        auth_set_app_key(config.app_key.clone())
            .await
            .map_err(|err| anyhow::anyhow!(err))?;

        let sdk = Arc::new(TriggerSdk::initialize(&config.db_path).with_context(|| {
            format!(
                "failed to initialize trigger sdk database at {}",
                config.db_path.display()
            )
        })?);

        Ok(Self {
            config,
            sdk,
            lock: Arc::new(Mutex::new(())),
        })
    }

    async fn refresh_thread_user_state(
        &self,
        thread_id: &str,
        user_state: &mut Value,
    ) -> Result<()> {
        let _guard = self.lock.lock().await;
        self.sync_best_effort_locked().await;
        let sdk_triggers = self.thread_triggers_from_snapshot_locked(thread_id)?;
        write_triggers_to_user_state(user_state, &sdk_triggers);
        Ok(())
    }

    async fn upsert_trigger(&self, draft: TriggerDraft) -> Result<TriggerItem> {
        let _guard = self.lock.lock().await;

        self.ensure_collection_locked(&draft.collection_uuid)?;
        self.upsert_request_thing_locked(&draft)?;
        self.register_trigger_locked(&draft)?;
        self.sdk.things_set_thing_trigger_uuid(
            &self.config.device_id,
            &draft.thing_uuid,
            Some(&draft.trigger_uuid),
        )?;
        self.sdk
            .upsert_trigger_binding(&draft.trigger_uuid, "thing", &draft.thing_uuid)?;
        self.sdk
            .set_trigger_paused(&draft.trigger_uuid, !draft.enabled)?;
        self.sync_best_effort_locked().await;

        let info = self
            .fetch_trigger_info_locked(&draft.trigger_uuid)?
            .ok_or_else(|| {
                anyhow::anyhow!("trigger missing after upsert: {}", draft.trigger_uuid)
            })?;
        Ok(trigger_item_from_parts(&draft, &info))
    }

    async fn delete_trigger(&self, item: &TriggerItem) -> Result<()> {
        let _guard = self.lock.lock().await;

        let _ = self
            .sdk
            .delete_trigger_and_bindings(&self.config.device_id, &item.trigger_uuid)?;
        let _ = self.sdk.things_delete_thing(
            &self.config.device_id,
            &item.collection_uuid,
            &item.thing_uuid,
        )?;

        let snapshot = self.snapshot_locked()?;
        let has_remaining_items = snapshot.things.iter().any(|thing| {
            thing.collection_uuid == item.collection_uuid
                && stored_attrs_from_thing(thing)
                    .map(|attrs| attrs.source == SDK_ATTRS_SOURCE)
                    .unwrap_or(false)
        });
        if !has_remaining_items {
            let _ = self
                .sdk
                .things_delete_collection(&self.config.device_id, &item.collection_uuid);
        }

        self.sync_best_effort_locked().await;
        Ok(())
    }

    fn ensure_collection_locked(&self, collection_uuid: &str) -> Result<()> {
        let collection = ThingCollectionUpsert {
            uuid: collection_uuid.to_string(),
            title: TRIGGER_COLLECTION_TITLE.to_string(),
            trigger_uuid: None,
            created_at: None,
            updated_at: None,
        };
        self.sdk.things_upsert_collection_json(
            &self.config.device_id,
            &serde_json::to_string(&collection)?,
        )?;
        Ok(())
    }

    fn upsert_request_thing_locked(&self, draft: &TriggerDraft) -> Result<()> {
        let payload = trigger_thing_payload(draft);
        self.sdk
            .things_upsert_thing_json(&self.config.device_id, &serde_json::to_string(&payload)?)?;
        self.patch_thing_attrs_locked(
            &draft.collection_uuid,
            &draft.thing_uuid,
            trigger_attrs_json(&draft),
        )?;
        Ok(())
    }

    fn register_trigger_locked(&self, draft: &TriggerDraft) -> Result<()> {
        self.sdk.register_trigger(TriggerRegistration {
            trigger_uuid: draft.trigger_uuid.clone(),
            name: draft.name.clone(),
            version: "1.0".to_string(),
            precondition: draft
                .precondition
                .iter()
                .map(trigger_rule_from_spec)
                .collect(),
            condition: draft.condition.iter().map(trigger_rule_from_spec).collect(),
        })?;
        Ok(())
    }

    fn fetch_trigger_info_locked(&self, trigger_uuid: &str) -> Result<Option<LocalTriggerInfo>> {
        Ok(self.list_trigger_info_map_locked()?.remove(trigger_uuid))
    }

    fn snapshot_locked(&self) -> Result<ThingsSnapshot> {
        let snapshot_json = self
            .sdk
            .things_list_snapshot_json_lite(&self.config.device_id)
            .context("failed to read local trigger snapshot")?;
        serde_json::from_str(&snapshot_json).context("failed to parse local trigger snapshot")
    }

    fn thread_triggers_from_snapshot_locked(&self, thread_id: &str) -> Result<Vec<TriggerItem>> {
        let snapshot = self.snapshot_locked()?;
        let trigger_map = self.list_trigger_info_map_locked()?;

        let mut triggers = Vec::new();
        for thing in &snapshot.things {
            let Some(attrs) = stored_attrs_from_thing(thing) else {
                continue;
            };
            if attrs.source != SDK_ATTRS_SOURCE || attrs.thread_id != thread_id {
                continue;
            }
            let Some(trigger_uuid) = thing.trigger_uuid.as_deref() else {
                continue;
            };
            let Some(info) = trigger_map.get(trigger_uuid) else {
                continue;
            };
            triggers.push(TriggerItem {
                id: attrs.local_id,
                trigger_uuid: info.trigger_uuid.clone(),
                thing_uuid: thing.uuid.clone(),
                collection_uuid: thing.collection_uuid.clone(),
                name: info.name.clone(),
                request: attrs.request.clone(),
                precondition: info.precondition.clone(),
                condition: info.condition.clone(),
                next_fire: info.next_fire.clone(),
                last_result: info.last_result,
                enabled: !info.is_paused,
                owner_user_id: attrs.owner_user_id.clone(),
                owner_username: attrs.owner_username.clone(),
                thread_id: attrs.thread_id.clone(),
            });
        }
        triggers.sort_by_key(|item| item.id);
        Ok(triggers)
    }

    fn list_trigger_info_map_locked(&self) -> Result<HashMap<String, LocalTriggerInfo>> {
        Ok(self
            .sdk
            .list_triggers()?
            .into_iter()
            .map(|info| {
                let trigger_uuid = info.trigger_id.clone();
                (
                    trigger_uuid.clone(),
                    LocalTriggerInfo {
                        trigger_uuid,
                        name: info.name,
                        precondition: info
                            .precondition
                            .iter()
                            .map(spec_from_trigger_rule)
                            .collect(),
                        condition: info.condition.iter().map(spec_from_trigger_rule).collect(),
                        next_fire: info.next_fire.map(|value| value.to_rfc3339()),
                        last_result: info.last_result,
                        is_paused: info.is_paused,
                    },
                )
            })
            .collect())
    }

    fn patch_thing_attrs_locked(
        &self,
        collection_uuid: &str,
        thing_uuid: &str,
        attrs_json: Value,
    ) -> Result<()> {
        let row = self
            .sdk
            .crdt_get_document(collection_uuid, "collection")?
            .ok_or_else(|| anyhow::anyhow!("collection document not found: {collection_uuid}"))?;
        let updated_doc = apply_collection_op(
            &row.automerge_doc,
            &self.config.device_id,
            collection_uuid,
            CollectionOp::UpsertThingMeta {
                thing_id: thing_uuid.to_string(),
                datatype: None,
                status: None,
                status_timestamp_ms: None,
                title: None,
                parent_id: None,
                trigger: TriggerUpdate::Noop,
                built_in: None,
                attrs_json: Some(serde_json::to_string(&attrs_json)?),
            },
        )?;
        self.sdk.crdt_save_document(
            collection_uuid,
            "collection",
            &updated_doc,
            &row.sync_state,
            true,
            row.last_sync_at.as_deref(),
        )?;
        Ok(())
    }

    async fn sync_best_effort_locked(&self) {
        if let Err(err) = sync_sdk_triggers(&self.sdk, &self.config).await {
            warn!(
                device_id = %self.config.device_id,
                error = %err,
                "sdk trigger sync failed; local state remains authoritative"
            );
        }
        if let Err(err) = self.reconcile_local_trigger_state_locked() {
            warn!(
                device_id = %self.config.device_id,
                error = %err,
                "sdk trigger reconciliation failed after sync"
            );
        }
    }

    fn reconcile_local_trigger_state_locked(&self) -> Result<()> {
        let _ = self
            .sdk
            .reconcile_trigger_bindings_after_sync(&self.config.device_id)?;
        let snapshot = self.snapshot_locked()?;
        let trigger_map = self.list_trigger_info_map_locked()?;

        for thing in &snapshot.things {
            let Some(trigger_uuid) = thing.trigger_uuid.as_deref() else {
                continue;
            };
            let Some(attrs) = stored_attrs_from_thing(thing) else {
                continue;
            };
            if attrs.source != SDK_ATTRS_SOURCE {
                continue;
            }

            let should_register = match trigger_map.get(trigger_uuid) {
                Some(info) => should_register_trigger(info, &attrs),
                None => true,
            };
            if should_register {
                let trigger_name = if attrs.name.trim().is_empty() {
                    thing.title.clone()
                } else {
                    attrs.name.clone()
                };
                self.sdk.register_trigger(TriggerRegistration {
                    trigger_uuid: trigger_uuid.to_string(),
                    name: trigger_name,
                    version: "1.0".to_string(),
                    precondition: attrs
                        .precondition
                        .iter()
                        .map(trigger_rule_from_spec)
                        .collect(),
                    condition: attrs.condition.iter().map(trigger_rule_from_spec).collect(),
                })?;
            }

            self.sdk
                .upsert_trigger_binding(trigger_uuid, "thing", &thing.uuid)?;
            if trigger_map
                .get(trigger_uuid)
                .map(|info| info.is_paused == attrs.enabled)
                .unwrap_or(true)
            {
                self.sdk.set_trigger_paused(trigger_uuid, !attrs.enabled)?;
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct TriggerDraft {
    id: u64,
    trigger_uuid: String,
    thing_uuid: String,
    collection_uuid: String,
    thread_id: String,
    name: String,
    request: String,
    precondition: Vec<TriggerRuleSpec>,
    condition: Vec<TriggerRuleSpec>,
    enabled: bool,
    owner_user_id: Option<String>,
    owner_username: Option<String>,
    reply_to_message_id: Option<String>,
    chat_type: Option<String>,
    platform: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalTriggerInfo {
    trigger_uuid: String,
    name: String,
    precondition: Vec<TriggerRuleSpec>,
    condition: Vec<TriggerRuleSpec>,
    next_fire: Option<String>,
    last_result: Option<bool>,
    is_paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSdkTriggerAttrs {
    source: String,
    thread_id: String,
    local_id: u64,
    #[serde(default)]
    name: String,
    request: String,
    #[serde(default)]
    precondition: Vec<TriggerRuleSpec>,
    #[serde(default)]
    condition: Vec<TriggerRuleSpec>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner_username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
}

fn default_true() -> bool {
    true
}

fn next_local_id(triggers: &[TriggerItem]) -> u64 {
    triggers.iter().map(|item| item.id).max().unwrap_or(0) + 1
}

fn ensure_trigger_tools_allowed(ctx: &ToolContext) -> Result<(), AgentError> {
    let enabled = ctx
        .metadata
        .as_ref()
        .and_then(|value| value.get(TRIGGER_TOOLS_ENABLED_META_KEY))
        .is_some_and(metadata_flag_enabled);
    if enabled {
        return Ok(());
    }

    Err(AgentError::tool(
        "trigger",
        "trigger tools are only available for the owner when remi sdk is configured",
    ))
}

fn store_user_state(ctx: &ToolContext, user_state: Value) {
    *ctx.user_state.write().unwrap() = user_state;
}

fn metadata_flag_enabled(value: &Value) -> bool {
    value.as_bool().unwrap_or_else(|| {
        value
            .as_str()
            .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "True"))
            .unwrap_or(false)
    })
}

fn metadata_string(ctx: &ToolContext, key: &str) -> Option<String> {
    ctx.metadata
        .as_ref()
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn thread_id_from_ctx<'a>(ctx: &'a ToolContext) -> Option<&'a str> {
    ctx.thread_id
        .as_ref()
        .map(|thread_id| thread_id.0.as_str())
        .or_else(|| {
            ctx.metadata
                .as_ref()
                .and_then(|value| value.get("thread_id"))
                .and_then(Value::as_str)
        })
}

fn default_device_id(data_dir: &Path) -> String {
    let stable_path = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    format!(
        "remi-cat-trigger-{}",
        Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            stable_path.to_string_lossy().as_bytes()
        )
    )
}

fn collection_uuid_for(thread_id: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("remi-cat-trigger-collection:{thread_id}").as_bytes(),
    )
    .to_string()
}

fn trigger_rule_from_spec(rule: &TriggerRuleSpec) -> TriggerRule {
    TriggerRule {
        rule: rule.rule.clone(),
        description: rule.description.clone(),
    }
}

fn spec_from_trigger_rule(rule: &TriggerRule) -> TriggerRuleSpec {
    TriggerRuleSpec {
        rule: rule.rule.clone(),
        description: rule.description.clone(),
    }
}

fn trigger_item_from_parts(draft: &TriggerDraft, info: &LocalTriggerInfo) -> TriggerItem {
    TriggerItem {
        id: draft.id,
        trigger_uuid: draft.trigger_uuid.clone(),
        thing_uuid: draft.thing_uuid.clone(),
        collection_uuid: draft.collection_uuid.clone(),
        name: info.name.clone(),
        request: draft.request.clone(),
        precondition: info.precondition.clone(),
        condition: info.condition.clone(),
        next_fire: info.next_fire.clone(),
        last_result: info.last_result,
        enabled: !info.is_paused,
        owner_user_id: draft.owner_user_id.clone(),
        owner_username: draft.owner_username.clone(),
        thread_id: draft.thread_id.clone(),
    }
}

fn trigger_thing_payload(draft: &TriggerDraft) -> ThingUpsert {
    ThingUpsert {
        uuid: draft.thing_uuid.clone(),
        title: draft.name.clone(),
        datatype: ThingDatatype::Text,
        data: Some(Value::String(draft.request.clone())),
        collection_uuid: draft.collection_uuid.clone(),
        trigger_uuid: None,
        parent_uuid: None,
        created_at: None,
        updated_at: None,
    }
}

fn stored_attrs_from_thing(
    thing: &remi_client_sdk::things_crdt::ThingEntry,
) -> Option<StoredSdkTriggerAttrs> {
    let attrs = thing.data.get("attrs")?;
    let attrs = attrs.get(SDK_ATTRS_ROOT_KEY)?;
    serde_json::from_value(attrs.clone()).ok()
}

fn trigger_attrs_json(draft: &TriggerDraft) -> Value {
    json!({
        SDK_ATTRS_ROOT_KEY: StoredSdkTriggerAttrs {
            source: SDK_ATTRS_SOURCE.to_string(),
            thread_id: draft.thread_id.clone(),
            local_id: draft.id,
            name: draft.name.clone(),
            request: draft.request.clone(),
            precondition: draft.precondition.clone(),
            condition: draft.condition.clone(),
            enabled: draft.enabled,
            owner_user_id: draft.owner_user_id.clone(),
            owner_username: draft.owner_username.clone(),
            reply_to_message_id: draft.reply_to_message_id.clone(),
            chat_type: draft.chat_type.clone(),
            platform: draft.platform.clone(),
        }
    })
}

fn should_register_trigger(info: &LocalTriggerInfo, attrs: &StoredSdkTriggerAttrs) -> bool {
    (!attrs.name.trim().is_empty() && info.name != attrs.name)
        || info.precondition != attrs.precondition
        || info.condition != attrs.condition
}

async fn sync_sdk_triggers(sdk: &TriggerSdk, config: &SdkTriggerConfig) -> Result<()> {
    let mut client = TriggerClient::new_with_shared_transport(String::new()).await?;
    let _ = sync_v3_documents_with_server(sdk, &mut client, &config.device_id).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{trigger_thing_payload, TriggerDraft};
    use crate::trigger::tools::TriggerRuleSpec;
    use serde_json::Value;

    #[test]
    fn trigger_payload_initializes_markdown_content() {
        let draft = TriggerDraft {
            id: 1,
            trigger_uuid: "trigger-1".to_string(),
            thing_uuid: "thing-1".to_string(),
            collection_uuid: "collection-1".to_string(),
            thread_id: "thread-1".to_string(),
            name: "Morning summary".to_string(),
            request: "Send the summary".to_string(),
            precondition: vec![TriggerRuleSpec {
                rule: "cron('0 9 * * *')".to_string(),
                description: "Every day at 09:00".to_string(),
            }],
            condition: Vec::new(),
            enabled: true,
            owner_user_id: None,
            owner_username: None,
            reply_to_message_id: None,
            chat_type: None,
            platform: None,
        };

        let payload = trigger_thing_payload(&draft);

        assert_eq!(payload.uuid, "thing-1");
        assert_eq!(payload.title, "Morning summary");
        assert_eq!(payload.data, Some(Value::String("Send the summary".to_string())));
    }
}
