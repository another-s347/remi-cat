use anyhow::Result;

use remi_things_crdt::{apply_op, extract_view, Block, Content, Op, ThingDatatype, TriggerUpdate};

#[test]
fn batch_upsert_matches_sequential_application() -> Result<()> {
    let actor = "device-a";

    let mut doc_seq: Vec<u8> = Vec::new();
    doc_seq = apply_op(
        &doc_seq,
        actor,
        Op::UpsertCollection {
            id: "c1".to_string(),
            title: Some("Inbox".to_string()),
            status: Some("active".to_string()),
            trigger: TriggerUpdate::Noop,
        },
    )?;
    doc_seq = apply_op(
        &doc_seq,
        actor,
        Op::UpsertThing {
            id: "t1".to_string(),
            collection_id: "c1".to_string(),
            datatype: Some(ThingDatatype::Markdown),
            status: Some("none".to_string()),
            status_timestamp_ms: None,
            title: Some("Hello".to_string()),
            parent_id: None,
            trigger: TriggerUpdate::Noop,
            content: Some(Content::Markdown {
                blocks: vec![Block {
                    id: "main".to_string(),
                    r#type: "markdown".to_string(),
                    attrs_json: None,
                    text: Some("hello".to_string()),
                }],
            }),
        },
    )?;

    let mut doc_batch: Vec<u8> = Vec::new();
    doc_batch = apply_op(
        &doc_batch,
        actor,
        Op::BatchUpsert {
            ops: vec![
                Op::UpsertCollection {
                    id: "c1".to_string(),
                    title: Some("Inbox".to_string()),
                    status: Some("active".to_string()),
                    trigger: TriggerUpdate::Noop,
                },
                Op::UpsertThing {
                    id: "t1".to_string(),
                    collection_id: "c1".to_string(),
                    datatype: Some(ThingDatatype::Markdown),
                    status: Some("none".to_string()),
                    status_timestamp_ms: None,
                    title: Some("Hello".to_string()),
                    parent_id: None,
                    trigger: TriggerUpdate::Noop,
                    content: Some(Content::Markdown {
                        blocks: vec![Block {
                            id: "main".to_string(),
                            r#type: "markdown".to_string(),
                            attrs_json: None,
                            text: Some("hello".to_string()),
                        }],
                    }),
                },
            ],
        },
    )?;

    let v_seq = extract_view(&doc_seq)?;
    let v_batch = extract_view(&doc_batch)?;
    assert_eq!(v_seq, v_batch);

    Ok(())
}
