use remi_things_crdt::{Block, Content, Op, ThingDatatype, TriggerUpdate};

#[test]
fn splice_text_missing_block_id_returns_error() {
    let actor = "device-a";

    // Create a thing whose markdown block id is NOT "main".
    let doc = remi_things_crdt::apply_op(
        &[],
        actor,
        Op::UpsertThing {
            id: "thing-1".to_string(),
            collection_id: "col-1".to_string(),
            datatype: Some(ThingDatatype::Markdown),
            status: None,
            status_timestamp_ms: None,
            title: Some("t".to_string()),
            parent_id: None,
            trigger: TriggerUpdate::Noop,
            content: Some(Content::Markdown {
                blocks: vec![Block {
                    id: "b1".to_string(),
                    r#type: "markdown".to_string(),
                    attrs_json: None,
                    text: Some("hello".to_string()),
                }],
            }),
        },
    )
    .expect("upsert should succeed");

    // Splice against a non-existent block must error (regression for silent Ok(())).
    let err = remi_things_crdt::apply_op(
        &doc,
        actor,
        Op::SpliceText {
            thing_id: "thing-1".to_string(),
            block_id: "main".to_string(),
            index: 0,
            delete: 0,
            insert: "X".to_string(),
        },
    )
    .expect_err("splice against missing block must return error");

    let msg = err.to_string();
    assert!(msg.contains("Block") && msg.contains("not found"), "{msg}");
}
