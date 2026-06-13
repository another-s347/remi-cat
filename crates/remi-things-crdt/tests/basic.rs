use remi_things_crdt::{
    apply_op, extract_view, materialize::materialize_plan, Block, Content, Op, ThingDatatype,
    TriggerUpdate,
};

#[test]
fn roundtrip_upsert_extract_materialize() {
    let mut doc: Vec<u8> = Vec::new();

    doc = apply_op(
        &doc,
        "device-a",
        Op::UpsertCollection {
            id: "c1".to_string(),
            title: Some("Inbox".to_string()),
            status: Some("active".to_string()),
            trigger: TriggerUpdate::Clear,
        },
    )
    .unwrap();

    doc = apply_op(
        &doc,
        "device-a",
        Op::UpsertThing {
            id: "t1".to_string(),
            collection_id: "c1".to_string(),
            datatype: Some(ThingDatatype::Text),
            status: Some("none".to_string()),
            status_timestamp_ms: None,
            title: Some("Hello".to_string()),
            parent_id: None,
            trigger: TriggerUpdate::Set("trig-123".to_string()),
            content: Some(Content::Text {
                blocks: vec![Block {
                    id: "b1".to_string(),
                    r#type: "paragraph".to_string(),
                    attrs_json: None,
                    text: Some("abc".to_string()),
                }],
            }),
        },
    )
    .unwrap();

    doc = apply_op(
        &doc,
        "device-a",
        Op::SpliceText {
            thing_id: "t1".to_string(),
            block_id: "b1".to_string(),
            index: 1,
            delete: 1,
            insert: "Z".to_string(),
        },
    )
    .unwrap();

    let view = extract_view(&doc).unwrap();
    assert_eq!(view.collections.len(), 1);
    assert_eq!(view.things.len(), 1);

    let thing = &view.things[0];
    assert_eq!(thing.id, "t1");

    let blocks = thing.content.as_ref().unwrap().blocks.as_ref().unwrap();
    assert_eq!(blocks[0].text.as_deref(), Some("aZc"));

    let plan = materialize_plan(&view);
    assert_eq!(plan.upsert_collections.len(), 1);
    assert_eq!(plan.upsert_things.len(), 1);
    assert_eq!(plan.set_bindings.len(), 1);
}
