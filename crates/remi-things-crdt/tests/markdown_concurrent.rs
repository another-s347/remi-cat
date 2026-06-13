use remi_things_crdt::{
    apply_op, decode_markdown_only_thing, extract_view, Block, Content, MarkdownOnlyDecoded, Op,
    ThingDatatype, TriggerUpdate,
};

use anyhow::Result;
use automerge::sync::SyncDoc;
use automerge::{sync, AutoCommit};

fn server_apply_and_reply(
    doc_bytes: &[u8],
    state: &mut sync::State,
    incoming: Option<Vec<u8>>,
    max_messages: usize,
) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    let mut doc = if doc_bytes.is_empty() {
        AutoCommit::new()
    } else {
        AutoCommit::load(doc_bytes)?
    };

    if let Some(incoming) = incoming {
        if !incoming.is_empty() {
            let msg = sync::Message::decode(&incoming)?;
            doc.sync().receive_sync_message(state, msg)?;
        }
    }

    let mut replies: Vec<Vec<u8>> = Vec::new();
    for _ in 0..max_messages {
        let next = doc.sync().generate_sync_message(state).map(|m| m.encode());
        match next {
            Some(bytes) if !bytes.is_empty() => replies.push(bytes),
            _ => break,
        }
    }

    Ok((doc.save(), replies))
}

fn client_apply_replies_and_next(
    doc_bytes: &[u8],
    state: &mut sync::State,
    replies: &[Vec<u8>],
) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let mut doc = if doc_bytes.is_empty() {
        AutoCommit::new()
    } else {
        AutoCommit::load(doc_bytes)?
    };

    for r in replies {
        if r.is_empty() {
            continue;
        }
        let msg = sync::Message::decode(r)?;
        doc.sync().receive_sync_message(state, msg)?;
    }

    let next = doc.sync().generate_sync_message(state).map(|m| m.encode());
    Ok((doc.save(), next))
}

fn converge_unary(
    mut client_doc: Vec<u8>,
    mut server_doc: Vec<u8>,
    max_round_trips: usize,
) -> Result<(Vec<u8>, Vec<u8>, usize)> {
    let mut client_state = sync::State::new();
    let mut server_state = sync::State::new();

    let mut next_client_msg: Option<Vec<u8>> = {
        let mut doc = if client_doc.is_empty() {
            AutoCommit::new()
        } else {
            AutoCommit::load(&client_doc)?
        };
        let mut st = client_state.clone();
        let msg = doc
            .sync()
            .generate_sync_message(&mut st)
            .map(|m| m.encode());
        msg
    };

    for round in 1..=max_round_trips {
        let (new_server_doc, replies) =
            server_apply_and_reply(&server_doc, &mut server_state, next_client_msg.take(), 32)?;
        server_doc = new_server_doc;

        let (new_client_doc, next_msg) =
            client_apply_replies_and_next(&client_doc, &mut client_state, &replies)?;
        client_doc = new_client_doc;

        if next_msg.is_none() && replies.is_empty() {
            return Ok((client_doc, server_doc, round));
        }

        next_client_msg = next_msg;
    }

    anyhow::bail!("Did not converge within {max_round_trips} round trips")
}

#[test]
fn concurrent_editing_markdown_converges() {
    // Seed base doc with markdown content.
    let mut base: Vec<u8> = Vec::new();
    base = apply_op(
        &base,
        "seed",
        Op::UpsertCollection {
            id: "c1".to_string(),
            title: Some("Inbox".to_string()),
            status: Some("active".to_string()),
            trigger: TriggerUpdate::Noop,
        },
    )
    .unwrap();

    base = apply_op(
        &base,
        "seed",
        Op::UpsertThing {
            id: "t1".to_string(),
            collection_id: "c1".to_string(),
            datatype: Some(ThingDatatype::Markdown),
            status: Some("none".to_string()),
            status_timestamp_ms: None,
            title: Some("Doc".to_string()),
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
    .unwrap();

    // Two peers edit different positions concurrently (non-conflicting).
    let mut doc_a = base.clone();
    doc_a = apply_op(
        &doc_a,
        "device-a",
        Op::SpliceText {
            thing_id: "t1".to_string(),
            block_id: "b1".to_string(),
            index: 5,
            delete: 0,
            insert: "A".to_string(),
        },
    )
    .unwrap();

    let mut doc_b = base.clone();
    doc_b = apply_op(
        &doc_b,
        "device-b",
        Op::SpliceText {
            thing_id: "t1".to_string(),
            block_id: "b1".to_string(),
            index: 0,
            delete: 0,
            insert: "B".to_string(),
        },
    )
    .unwrap();

    let (a_final, b_final, rounds) = converge_unary(doc_a, doc_b, 10).unwrap();
    assert!(rounds <= 6, "expected convergence, got {rounds} rounds");

    let v_a = extract_view(&a_final).unwrap();
    let v_b = extract_view(&b_final).unwrap();
    assert_eq!(v_a, v_b);

    let t1 = v_a.things.iter().find(|t| t.id == "t1").unwrap();
    match decode_markdown_only_thing(t1).unwrap() {
        MarkdownOnlyDecoded::Markdown { text } => {
            assert_eq!(text, "BhelloA");
        }
        other => panic!("unexpected decode: {other:?}"),
    }
}
