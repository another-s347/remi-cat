use anyhow::{Context, Result};
use remi_client_sdk::TriggerSdk;
use remi_client_sdk::things_crdt::{ThingCollectionUpsert, ThingDatatype, ThingUpsert};
use remi_things_crdt::{ROOT_DOC_UUID, extract_collection_doc_view, extract_root_view};

fn temp_db_path() -> Result<String> {
    let dir = tempfile::tempdir().context("tempdir")?;
    let path = dir.path().join("remi-sdk-test.sqlite3");
    let path_str = path.to_string_lossy().to_string();
    std::mem::forget(dir);
    Ok(path_str)
}

fn parse_snapshot(sdk: &TriggerSdk, device_id: &str) -> Result<serde_json::Value> {
    let snapshot = sdk.things_list_snapshot(device_id)?;
    serde_json::to_value(snapshot).context("serialize snapshot")
}

#[test]
fn delete_collection_preserves_tombstone_and_does_not_resurrect_on_reload() -> Result<()> {
    let db_path = temp_db_path()?;
    let sdk = TriggerSdk::initialize(&db_path).context("init sdk")?;
    let device_id = "device-a";

    sdk.things_upsert_collection(
        device_id,
        ThingCollectionUpsert {
            uuid: "c1".to_string(),
            title: "Inbox".to_string(),
            trigger_uuid: None,
            created_at: None,
            updated_at: None,
        },
    )?;
    sdk.things_upsert_thing(
        device_id,
        ThingUpsert {
            uuid: "t1".to_string(),
            title: "Thing 1".to_string(),
            datatype: ThingDatatype::Markdown,
            data: Some(serde_json::json!({"markdown": "hello world"})),
            collection_uuid: "c1".to_string(),
            trigger_uuid: None,
            parent_uuid: None,
            created_at: None,
            updated_at: None,
        },
    )?;

    let before = parse_snapshot(&sdk, device_id)?;
    assert_eq!(before["collections"].as_array().unwrap().len(), 1);
    assert_eq!(before["things"].as_array().unwrap().len(), 1);

    assert!(sdk.things_delete_collection(device_id, "c1")?);

    let after = parse_snapshot(&sdk, device_id)?;
    assert_eq!(after["collections"].as_array().unwrap().len(), 0);
    assert_eq!(after["things"].as_array().unwrap().len(), 0);

    let root_row = sdk
        .crdt_get_document(ROOT_DOC_UUID, "root")?
        .context("root document should exist")?;
    let root_view = extract_root_view(&root_row.automerge_doc)?;
    assert!(!root_view.collection_uuids.contains(&"c1".to_string()));

    let collection_row = sdk
        .crdt_get_document("c1", "collection")?
        .context("collection document should be retained for tombstone sync")?;
    let collection_view = extract_collection_doc_view(&collection_row.automerge_doc, "c1")?;
    assert!(
        collection_view
            .meta
            .tombstone
            .as_ref()
            .map(|t| t.deleted)
            .unwrap_or(false)
    );

    assert!(sdk.crdt_get_document("t1", "thing_markdown")?.is_some());

    drop(sdk);

    let sdk = TriggerSdk::initialize(&db_path).context("re-init sdk")?;
    let reloaded = parse_snapshot(&sdk, device_id)?;
    assert_eq!(reloaded["collections"].as_array().unwrap().len(), 0);
    assert_eq!(reloaded["things"].as_array().unwrap().len(), 0);
    assert_eq!(sdk.things_get_thing_markdown(device_id, "t1")?, None);

    Ok(())
}

#[test]
fn delete_thing_keeps_markdown_doc_but_hides_content_after_reload() -> Result<()> {
    let db_path = temp_db_path()?;
    let sdk = TriggerSdk::initialize(&db_path).context("init sdk")?;
    let device_id = "device-a";

    sdk.things_upsert_collection(
        device_id,
        ThingCollectionUpsert {
            uuid: "c1".to_string(),
            title: "Inbox".to_string(),
            trigger_uuid: None,
            created_at: None,
            updated_at: None,
        },
    )?;
    sdk.things_upsert_thing(
        device_id,
        ThingUpsert {
            uuid: "t1".to_string(),
            title: "Thing 1".to_string(),
            datatype: ThingDatatype::Markdown,
            data: Some(serde_json::json!({"markdown": "hello world"})),
            collection_uuid: "c1".to_string(),
            trigger_uuid: None,
            parent_uuid: None,
            created_at: None,
            updated_at: None,
        },
    )?;

    assert!(sdk.things_delete_thing(device_id, "c1", "t1")?);

    let after = parse_snapshot(&sdk, device_id)?;
    assert_eq!(after["things"].as_array().unwrap().len(), 0);
    assert!(sdk.crdt_get_document("t1", "thing_markdown")?.is_some());
    assert_eq!(sdk.things_get_thing_markdown(device_id, "t1")?, None);

    drop(sdk);

    let sdk = TriggerSdk::initialize(&db_path).context("re-init sdk")?;
    let reloaded = parse_snapshot(&sdk, device_id)?;
    assert_eq!(reloaded["things"].as_array().unwrap().len(), 0);
    assert_eq!(sdk.things_get_thing_markdown(device_id, "t1")?, None);

    let collection_row = sdk
        .crdt_get_document("c1", "collection")?
        .context("collection document should exist")?;
    let collection_view = extract_collection_doc_view(&collection_row.automerge_doc, "c1")?;
    let thing = collection_view
        .things
        .iter()
        .find(|thing| thing.id == "t1")
        .context("thing metadata should remain for tombstone sync")?;
    assert!(thing.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false));

    Ok(())
}
