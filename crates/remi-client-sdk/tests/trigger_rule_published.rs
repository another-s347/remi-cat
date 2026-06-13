use remi_client_sdk::things_crdt::ThingCollectionUpsert;
use remi_client_sdk::things_handlers::TriggerRulePublishedHandler;
use remi_client_sdk::{ExternalToolHandler, TriggerRegistration, TriggerRule, TriggerSdk};
use serde_json::json;
use std::sync::Arc;
use tempfile::tempdir;

fn build_rule_config(cron_expr: &str) -> serde_json::Value {
    json!({
        "precondition": [{
            "rule": format!("cron('{}')", cron_expr),
            "description": "cron",
        }],
        "condition": [],
    })
}

fn build_registration(trigger_uuid: &str, name: &str, cron_expr: &str) -> TriggerRegistration {
    TriggerRegistration {
        trigger_uuid: trigger_uuid.to_string(),
        name: name.to_string(),
        version: "1.0".to_string(),
        precondition: vec![TriggerRule {
            rule: format!("cron('{}')", cron_expr),
            description: "cron".to_string(),
        }],
        condition: Vec::new(),
        action_uuid: None,
        action_args: json!({}),
    }
}

#[tokio::test]
async fn trigger_rule_published_rebind_deletes_old_trigger() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("events.db");
    let sdk = Arc::new(TriggerSdk::initialize(&db_path).expect("sdk init"));
    let device_id = "device-1".to_string();
    let collection_uuid = "collection-1";

    sdk.things_upsert_collection(
        &device_id,
        ThingCollectionUpsert {
            uuid: collection_uuid.to_string(),
            title: "Inbox".to_string(),
            trigger_uuid: None,
            created_at: None,
            updated_at: None,
        },
    )
    .expect("insert collection");

    let old_trigger_uuid = "trigger-old";
    let old_registration = build_registration(old_trigger_uuid, "Old trigger", "0 9 * * *");
    sdk.register_trigger(old_registration)
        .expect("register old trigger");
    sdk.things_set_collection_trigger_uuid(&device_id, collection_uuid, Some(old_trigger_uuid))
        .expect("bind old trigger");
    sdk.upsert_trigger_binding(old_trigger_uuid, "collection", collection_uuid)
        .expect("insert old binding");

    let handler = TriggerRulePublishedHandler::new(sdk.clone(), device_id.clone());
    let new_trigger_uuid = "trigger-new";
    let payload = json!({
        "type": "trigger_rule_published",
        "trigger_uuid": new_trigger_uuid,
        "name": "New trigger",
        "rule_config_json": build_rule_config("0 10 * * *"),
        "bind_uuid": collection_uuid,
        "bind_type": "collection"
    });

    handler
        .handle("interrupt-1", &payload)
        .await
        .expect("handle trigger publish");

    let triggers = sdk.list_triggers().expect("list triggers");
    let trigger_ids: Vec<String> = triggers.into_iter().map(|t| t.trigger_id).collect();
    assert!(trigger_ids.contains(&new_trigger_uuid.to_string()));
    assert!(!trigger_ids.contains(&old_trigger_uuid.to_string()));

    let snapshot = sdk.things_list_snapshot(&device_id).expect("snapshot");
    let collection = snapshot
        .collections
        .iter()
        .find(|c| c.uuid == collection_uuid)
        .expect("collection exists");
    assert_eq!(collection.trigger_uuid.as_deref(), Some(new_trigger_uuid));
}

#[tokio::test]
async fn trigger_rule_published_missing_entity_does_not_register() {
    let dir = tempdir().expect("tempdir");
    let db_path = dir.path().join("events.db");
    let sdk = Arc::new(TriggerSdk::initialize(&db_path).expect("sdk init"));
    let device_id = "device-2".to_string();

    let handler = TriggerRulePublishedHandler::new(sdk.clone(), device_id.clone());
    let payload = json!({
        "type": "trigger_rule_published",
        "trigger_uuid": "trigger-missing",
        "name": "Missing target",
        "rule_config_json": build_rule_config("0 11 * * *"),
        "bind_uuid": "missing-collection",
        "bind_type": "collection"
    });

    let result = handler.handle("interrupt-2", &payload).await;
    assert!(result.is_err());

    let triggers = sdk.list_triggers().expect("list triggers");
    assert!(triggers.is_empty());
}
