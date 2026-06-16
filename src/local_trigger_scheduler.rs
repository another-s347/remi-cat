use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use remi_client_sdk::things_crdt::ThingEntry;
use remi_client_sdk::{TriggerCallback, TriggerExecutionSummary, TriggerSdk};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

const SDK_ATTRS_ROOT_KEY: &str = "remi_cat_trigger";
const SDK_ATTRS_SOURCE: &str = "remi-cat-trigger";
const SDK_DB_FILE_NAME: &str = "trigger-sdk.db";
const POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) struct LocalTriggerDispatch {
    pub trigger_uuid: String,
    pub trigger_name: String,
    pub thread_id: String,
    pub request: String,
    pub owner_user_id: String,
    pub owner_username: Option<String>,
    pub chat_type: Option<String>,
    pub platform: Option<String>,
}

pub(crate) fn spawn_local_trigger_scheduler(
    data_dir: PathBuf,
    dispatch_tx: mpsc::UnboundedSender<LocalTriggerDispatch>,
) {
    tokio::spawn(async move {
        loop {
            if dispatch_tx.is_closed() {
                info!("local trigger scheduler stopping because dispatch channel closed");
                return;
            }
            if let Err(err) = tick(&data_dir, &dispatch_tx).await {
                warn!(error = %err, "local trigger scheduler tick failed");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

async fn tick(
    data_dir: &Path,
    dispatch_tx: &mpsc::UnboundedSender<LocalTriggerDispatch>,
) -> Result<()> {
    for db_path in discover_trigger_dbs(data_dir)? {
        let user_key = db_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .unwrap_or("anonymous")
            .to_string();
        let device_id = default_device_id(data_dir, &user_key);
        let sdk = TriggerSdk::initialize(&db_path)
            .with_context(|| format!("open trigger sdk db {}", db_path.display()))?;
        let snapshot = sdk
            .things_list_snapshot_lite(&device_id)
            .context("read local trigger snapshot")?;
        let summaries = sdk.run_due_triggers(&NoopTriggerCallback)?;
        for summary in summaries.into_iter().filter(|summary| summary.result) {
            let Some(dispatch) = dispatch_from_summary(&sdk, &snapshot.things, &summary)? else {
                continue;
            };
            if dispatch_tx.send(dispatch).is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn discover_trigger_dbs(data_dir: &Path) -> Result<Vec<PathBuf>> {
    let sdk_root = data_dir.join("sdk");
    let mut dbs = Vec::new();
    if !sdk_root.is_dir() {
        return Ok(dbs);
    }
    for entry in std::fs::read_dir(&sdk_root)
        .with_context(|| format!("read sdk root {}", sdk_root.display()))?
    {
        let entry = entry?;
        let db_path = entry.path().join("trigger").join(SDK_DB_FILE_NAME);
        if db_path.is_file() {
            dbs.push(db_path);
        }
    }
    Ok(dbs)
}

fn dispatch_from_summary(
    sdk: &TriggerSdk,
    things: &[ThingEntry],
    summary: &TriggerExecutionSummary,
) -> Result<Option<LocalTriggerDispatch>> {
    let (thing_uuids, _) = sdk.get_bound_entities_for_trigger_api(&summary.trigger_id)?;
    let Some(thing) = thing_uuids
        .iter()
        .find_map(|thing_uuid| things.iter().find(|thing| thing.uuid == *thing_uuid))
    else {
        warn!(trigger_uuid = %summary.trigger_id, "local trigger fired without a bound thing");
        return Ok(None);
    };
    let Some(attrs) = stored_attrs_from_thing(thing) else {
        warn!(trigger_uuid = %summary.trigger_id, thing_uuid = %thing.uuid, "local trigger thing is missing remi-cat attrs");
        return Ok(None);
    };
    if attrs.source != SDK_ATTRS_SOURCE {
        return Ok(None);
    }
    if attrs.request.trim().is_empty() {
        warn!(trigger_uuid = %summary.trigger_id, thing_uuid = %thing.uuid, "local trigger request is empty");
        return Ok(None);
    }
    let Some(owner_user_id) = attrs
        .owner_user_id
        .clone()
        .filter(|value| !value.trim().is_empty())
    else {
        warn!(trigger_uuid = %summary.trigger_id, "local trigger missing owner user id");
        return Ok(None);
    };

    Ok(Some(LocalTriggerDispatch {
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
        thread_id: attrs.thread_id,
        request: attrs.request,
        owner_user_id,
        owner_username: attrs.owner_username,
        chat_type: attrs.chat_type,
        platform: attrs.platform,
    }))
}

fn stored_attrs_from_thing(thing: &ThingEntry) -> Option<StoredSdkTriggerAttrs> {
    let attrs = thing.data.get("attrs")?;
    let attrs = attrs.get(SDK_ATTRS_ROOT_KEY)?;
    serde_json::from_value(attrs.clone()).ok()
}

#[derive(Debug, Clone, Deserialize)]
struct StoredSdkTriggerAttrs {
    source: String,
    thread_id: String,
    #[serde(default)]
    name: String,
    request: String,
    #[serde(default)]
    owner_user_id: Option<String>,
    #[serde(default)]
    owner_username: Option<String>,
    #[serde(default)]
    chat_type: Option<String>,
    #[serde(default)]
    platform: Option<String>,
}

#[derive(Debug, Clone)]
struct NoopTriggerCallback;

impl TriggerCallback for NoopTriggerCallback {
    fn on_trigger(&self, _summary: &TriggerExecutionSummary) -> Result<()> {
        Ok(())
    }
}

fn default_device_id(data_dir: &Path, user_key: &str) -> String {
    let stable_path = data_dir
        .canonicalize()
        .unwrap_or_else(|_| data_dir.to_path_buf());
    format!(
        "remi-cat-trigger-{}",
        Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("{}:{user_key}", stable_path.to_string_lossy()).as_bytes()
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use remi_client_sdk::things_crdt::{ThingCollectionUpsert, ThingDatatype, ThingUpsert};
    use remi_client_sdk::{TriggerRegistration, TriggerRule};
    use remi_things_crdt::{apply_collection_op, CollectionOp, TriggerUpdate};
    use serde_json::json;

    #[tokio::test]
    async fn scheduler_dispatches_due_local_timer_trigger() {
        let data_dir = std::env::temp_dir().join(format!(
            "remi-local-trigger-scheduler-test-{}",
            Uuid::new_v4()
        ));
        let user_key = "web-local";
        let db_dir = data_dir.join("sdk").join(user_key).join("trigger");
        std::fs::create_dir_all(&db_dir).expect("create trigger db dir");
        let db_path = db_dir.join(SDK_DB_FILE_NAME);
        let device_id = default_device_id(&data_dir, user_key);
        let sdk = TriggerSdk::initialize(&db_path).expect("init sdk");

        let trigger_uuid = Uuid::new_v4().to_string();
        let collection_uuid = Uuid::new_v4().to_string();
        let thing_uuid = Uuid::new_v4().to_string();
        sdk.things_upsert_collection(
            &device_id,
            ThingCollectionUpsert {
                uuid: collection_uuid.clone(),
                title: "Semantic triggers".to_string(),
                trigger_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("upsert collection");
        sdk.things_upsert_thing(
            &device_id,
            ThingUpsert {
                uuid: thing_uuid.clone(),
                title: "Check job".to_string(),
                datatype: ThingDatatype::Text,
                data: Some(json!("Check the job now.")),
                collection_uuid: collection_uuid.clone(),
                trigger_uuid: None,
                parent_uuid: None,
                created_at: None,
                updated_at: None,
            },
        )
        .expect("upsert thing");
        sdk.things_set_thing_trigger_uuid(&device_id, &thing_uuid, Some(&trigger_uuid))
            .expect("set thing trigger uuid");
        let row = sdk
            .crdt_get_document(&collection_uuid, "collection")
            .expect("read collection doc")
            .expect("collection doc exists");
        let updated_doc = apply_collection_op(
            &row.automerge_doc,
            &device_id,
            &collection_uuid,
            CollectionOp::UpsertThingMeta {
                thing_id: thing_uuid.clone(),
                datatype: None,
                status: None,
                status_timestamp_ms: None,
                title: None,
                parent_id: None,
                trigger: TriggerUpdate::Noop,
                built_in: None,
                attrs_json: Some(
                    json!({
                        SDK_ATTRS_ROOT_KEY: {
                            "source": SDK_ATTRS_SOURCE,
                            "thread_id": "thread-1",
                            "local_id": 1,
                            "name": "Check job",
                            "request": "Check the job now.",
                            "owner_user_id": "web-local",
                            "owner_username": "web-local",
                            "chat_type": "p2p",
                            "platform": "web"
                        }
                    })
                    .to_string(),
                ),
            },
        )
        .expect("patch attrs");
        sdk.crdt_save_document(
            &collection_uuid,
            "collection",
            &updated_doc,
            &row.sync_state,
            true,
            row.last_sync_at.as_deref(),
        )
        .expect("save attrs");
        sdk.register_trigger(TriggerRegistration {
            trigger_uuid: trigger_uuid.clone(),
            name: "Check job".to_string(),
            version: "1.0".to_string(),
            precondition: vec![TriggerRule {
                rule: "timer('0s')".to_string(),
                description: "now".to_string(),
            }],
            condition: vec![TriggerRule {
                rule: "true".to_string(),
                description: "always".to_string(),
            }],
            action_uuid: None,
            action_args: json!({}),
        })
        .expect("register trigger");
        sdk.upsert_trigger_binding(&trigger_uuid, "thing", &thing_uuid)
            .expect("bind trigger");

        drop(sdk);
        let (tx, mut rx) = mpsc::unbounded_channel();
        tick(&data_dir, &tx).await.expect("scheduler tick");
        let dispatch = rx.try_recv().expect("dispatch should be emitted");

        assert_eq!(dispatch.trigger_uuid, trigger_uuid);
        assert_eq!(dispatch.trigger_name, "Check job");
        assert_eq!(dispatch.thread_id, "thread-1");
        assert_eq!(dispatch.request, "Check the job now.");
        assert_eq!(dispatch.owner_user_id, "web-local");
        assert_eq!(dispatch.platform.as_deref(), Some("web"));

        let _ = std::fs::remove_dir_all(data_dir);
    }
}
