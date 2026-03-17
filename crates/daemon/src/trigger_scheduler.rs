use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use remi_client_sdk::auth::auth_set_app_key;
use remi_client_sdk::things_crdt::{ThingEntry, ThingsSnapshot};
use remi_client_sdk::things_sync::sync_v3_documents_with_server;
use remi_client_sdk::transport::configure_shared_transport;
use remi_client_sdk::{
    TriggerCallback, TriggerClient, TriggerExecutionSummary, TriggerRegistration, TriggerRule,
    TriggerSdk,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

const REMI_APP_KEY_ENV: &str = "REMI_APP_KEY";
const REMI_PUBLIC_GRPC_ADDR_ENV: &str = "REMI_PUBLIC_GRPC_ADDR";
const REMI_DAEMON_TRIGGER_DEVICE_ID_ENV: &str = "REMI_DAEMON_TRIGGER_DEVICE_ID";
const SDK_ATTRS_ROOT_KEY: &str = "remi_cat_trigger";
const SDK_ATTRS_SOURCE: &str = "remi-cat-trigger";
const SDK_DB_FILE_NAME: &str = "trigger-daemon-sdk.db";
const SYNC_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct TriggerDispatch {
    pub trigger_uuid: String,
    pub trigger_name: String,
    pub chat_id: String,
    pub request: String,
    pub owner_user_id: Option<String>,
    pub owner_username: Option<String>,
    pub reply_to_message_id: Option<String>,
    pub chat_type: Option<String>,
    pub platform: Option<String>,
}

pub fn spawn_trigger_scheduler(data_dir: PathBuf, dispatch_tx: mpsc::Sender<TriggerDispatch>) {
    let Some(config) = SchedulerConfig::from_env(&data_dir) else {
        info!("trigger scheduler disabled because remi sdk is not fully configured");
        return;
    };

    tokio::spawn(async move {
        let runtime = match SchedulerRuntime::initialize(config).await {
            Ok(runtime) => runtime,
            Err(err) => {
                warn!(error = %err, "failed to initialize trigger scheduler runtime");
                return;
            }
        };

        runtime.run(dispatch_tx).await;
    });
}

#[derive(Debug, Clone)]
struct SchedulerConfig {
    app_key: String,
    public_grpc_addr: String,
    device_id: String,
    db_path: PathBuf,
}

impl SchedulerConfig {
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
                device_id: std::env::var(REMI_DAEMON_TRIGGER_DEVICE_ID_ENV)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| default_device_id(data_dir)),
                db_path: data_dir.join(SDK_DB_FILE_NAME),
            }),
            (None, None) => None,
            _ => {
                warn!(
                    "partial remi trigger scheduler configuration detected; set both {} and {}",
                    REMI_APP_KEY_ENV, REMI_PUBLIC_GRPC_ADDR_ENV,
                );
                None
            }
        }
    }
}

struct SchedulerRuntime {
    config: SchedulerConfig,
    sdk: TriggerSdk,
}

impl SchedulerRuntime {
    async fn initialize(config: SchedulerConfig) -> Result<Self> {
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

        let sdk = TriggerSdk::initialize(&config.db_path).with_context(|| {
            format!(
                "failed to initialize daemon trigger sdk database at {}",
                config.db_path.display()
            )
        })?;

        Ok(Self { config, sdk })
    }

    async fn run(self, dispatch_tx: mpsc::Sender<TriggerDispatch>) {
        loop {
            match self.tick(&dispatch_tx).await {
                Ok(wait) => tokio::time::sleep(wait).await,
                Err(err) => {
                    warn!(error = %err, "trigger scheduler tick failed");
                    tokio::time::sleep(SYNC_INTERVAL).await;
                }
            }
        }
    }

    async fn tick(&self, dispatch_tx: &mpsc::Sender<TriggerDispatch>) -> Result<Duration> {
        sync_sdk_triggers(&self.sdk, &self.config).await?;
        self.reconcile_local_trigger_state()?;

        let snapshot = self.snapshot()?;
        let summaries = self.sdk.run_due_triggers(&NoopTriggerCallback)?;
        for summary in summaries.into_iter().filter(|summary| summary.result) {
            let Some(dispatch) = self.dispatch_from_summary(&snapshot, &summary)? else {
                continue;
            };
            if dispatch_tx.send(dispatch).await.is_err() {
                info!("trigger scheduler stopping because dispatch channel closed");
                return Ok(SYNC_INTERVAL);
            }
        }

        self.next_wait_duration()
    }

    fn reconcile_local_trigger_state(&self) -> Result<()> {
        let _ = self
            .sdk
            .reconcile_trigger_bindings_after_sync(&self.config.device_id)?;
        let snapshot = self.snapshot()?;
        let trigger_map = self.list_trigger_info_map()?;

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

    fn dispatch_from_summary(
        &self,
        snapshot: &ThingsSnapshot,
        summary: &TriggerExecutionSummary,
    ) -> Result<Option<TriggerDispatch>> {
        let (thing_uuids, _) = self
            .sdk
            .get_bound_entities_for_trigger_api(&summary.trigger_id)?;
        let Some(thing) = thing_uuids.iter().find_map(|thing_uuid| {
            snapshot
                .things
                .iter()
                .find(|thing| thing.uuid == *thing_uuid)
        }) else {
            warn!(trigger_uuid = %summary.trigger_id, "trigger fired without a bound thing");
            return Ok(None);
        };

        let Some(attrs) = stored_attrs_from_thing(thing) else {
            warn!(trigger_uuid = %summary.trigger_id, thing_uuid = %thing.uuid, "trigger thing is missing remi-cat attrs");
            return Ok(None);
        };
        if attrs.source != SDK_ATTRS_SOURCE {
            return Ok(None);
        }
        if attrs.request.trim().is_empty() {
            warn!(trigger_uuid = %summary.trigger_id, thing_uuid = %thing.uuid, "trigger request is empty; skipping execution");
            return Ok(None);
        }

        Ok(Some(TriggerDispatch {
            trigger_uuid: summary.trigger_id.clone(),
            trigger_name: if summary.name.trim().is_empty() {
                if attrs.name.trim().is_empty() {
                    thing.title.clone()
                } else {
                    attrs.name.clone()
                }
            } else {
                summary.name.clone()
            },
            chat_id: attrs.thread_id.clone(),
            request: attrs.request.clone(),
            owner_user_id: attrs.owner_user_id.clone(),
            owner_username: attrs.owner_username.clone(),
            reply_to_message_id: attrs.reply_to_message_id.clone(),
            chat_type: attrs.chat_type.clone(),
            platform: attrs.platform.clone(),
        }))
    }

    fn list_trigger_info_map(&self) -> Result<HashMap<String, LocalTriggerInfo>> {
        Ok(self
            .sdk
            .list_triggers()?
            .into_iter()
            .map(|info| {
                let trigger_uuid = info.trigger_id.clone();
                (
                    trigger_uuid.clone(),
                    LocalTriggerInfo {
                        name: info.name,
                        precondition: info
                            .precondition
                            .iter()
                            .map(spec_from_trigger_rule)
                            .collect(),
                        condition: info.condition.iter().map(spec_from_trigger_rule).collect(),
                        is_paused: info.is_paused,
                    },
                )
            })
            .collect())
    }

    fn next_wait_duration(&self) -> Result<Duration> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let wait_until_next_fire = self
            .sdk
            .next_deadline(Some(now_secs))?
            .map(|deadline| {
                let delta = (deadline.timestamp() - now_secs).max(1) as u64;
                Duration::from_secs(delta)
            })
            .unwrap_or(SYNC_INTERVAL);

        Ok(wait_until_next_fire.min(SYNC_INTERVAL))
    }

    fn snapshot(&self) -> Result<ThingsSnapshot> {
        let snapshot_json = self
            .sdk
            .things_list_snapshot_json_lite(&self.config.device_id)
            .context("failed to read daemon trigger snapshot")?;
        serde_json::from_str(&snapshot_json).context("failed to parse daemon trigger snapshot")
    }
}

#[derive(Debug, Clone)]
struct NoopTriggerCallback;

impl TriggerCallback for NoopTriggerCallback {
    fn on_trigger(&self, _summary: &TriggerExecutionSummary) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct LocalTriggerInfo {
    name: String,
    precondition: Vec<TriggerRuleSpec>,
    condition: Vec<TriggerRuleSpec>,
    is_paused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TriggerRuleSpec {
    rule: String,
    description: String,
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

fn default_device_id(data_dir: &Path) -> String {
    let stable_path = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    format!(
        "remi-daemon-trigger-{}",
        Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            stable_path.to_string_lossy().as_bytes()
        )
    )
}

fn stored_attrs_from_thing(thing: &ThingEntry) -> Option<StoredSdkTriggerAttrs> {
    let attrs = thing.data.get("attrs")?;
    let attrs = attrs.get(SDK_ATTRS_ROOT_KEY)?;
    serde_json::from_value(attrs.clone()).ok()
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

fn should_register_trigger(info: &LocalTriggerInfo, attrs: &StoredSdkTriggerAttrs) -> bool {
    (!attrs.name.trim().is_empty() && info.name != attrs.name)
        || info.precondition != attrs.precondition
        || info.condition != attrs.condition
}

async fn sync_sdk_triggers(sdk: &TriggerSdk, config: &SchedulerConfig) -> Result<()> {
    let mut client = TriggerClient::new_with_shared_transport(String::new()).await?;
    let _ = sync_v3_documents_with_server(sdk, &mut client, &config.device_id).await?;
    Ok(())
}
