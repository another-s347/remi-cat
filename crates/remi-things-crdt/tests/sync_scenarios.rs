use anyhow::Result;
use automerge::sync::SyncDoc;
use automerge::{sync, AutoCommit};

use remi_things_crdt::{apply_op, extract_view, Block, Content, Op, ThingDatatype, TriggerUpdate};

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

/// Simulate unary-RPC style sync:
/// client sends 0/1 message per round; server replies with 0..N messages.
/// Returns (final_client_doc, final_server_doc, round_trips).
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

        // Converged when client has nothing new to say and server had nothing to say.
        if next_msg.is_none() && replies.is_empty() {
            return Ok((client_doc, server_doc, round));
        }

        next_client_msg = next_msg;
    }

    anyhow::bail!("Did not converge within {max_round_trips} round trips")
}

#[test]
fn scenario_1_fast_cold_start_bootstrap_client_from_server() {
    // Server already has content; client cold-starts empty.
    let mut server_doc: Vec<u8> = Vec::new();

    server_doc = apply_op(
        &server_doc,
        "server",
        Op::UpsertCollection {
            id: "c1".to_string(),
            title: Some("Inbox".to_string()),
            status: Some("none".to_string()),
            trigger: TriggerUpdate::Noop,
        },
    )
    .unwrap();

    server_doc = apply_op(
        &server_doc,
        "server",
        Op::UpsertThing {
            id: "t_server".to_string(),
            collection_id: "c1".to_string(),
            datatype: Some(ThingDatatype::Text),
            status: Some("none".to_string()),
            status_timestamp_ms: None,
            title: Some("Server Thing".to_string()),
            parent_id: None,
            trigger: TriggerUpdate::Noop,
            content: Some(Content::Text {
                blocks: vec![Block {
                    id: "b1".to_string(),
                    r#type: "paragraph".to_string(),
                    attrs_json: None,
                    text: Some("hello".to_string()),
                }],
            }),
        },
    )
    .unwrap();

    let client_doc: Vec<u8> = Vec::new();

    let (client_final, server_final, rounds) = converge_unary(client_doc, server_doc, 8).unwrap();

    // “快速冷启动”: should converge in a small number of unary round trips.
    assert!(rounds <= 3, "expected fast bootstrap, got {rounds} rounds");

    let v_client = extract_view(&client_final).unwrap();
    let v_server = extract_view(&server_final).unwrap();
    assert_eq!(v_client, v_server);
    assert_eq!(v_client.collections.len(), 1);
    assert_eq!(v_client.things.len(), 1);
    assert_eq!(v_client.things[0].id, "t_server");
}

#[test]
fn scenario_2_server_has_records_client_adds_before_cold_start_then_sync_merge() {
    // Server has existing record.
    let mut server_doc: Vec<u8> = Vec::new();
    server_doc = apply_op(
        &server_doc,
        "server",
        Op::UpsertCollection {
            id: "c1".to_string(),
            title: Some("Inbox".to_string()),
            status: Some("none".to_string()),
            trigger: TriggerUpdate::Noop,
        },
    )
    .unwrap();
    server_doc = apply_op(
        &server_doc,
        "server",
        Op::UpsertThing {
            id: "t_server".to_string(),
            collection_id: "c1".to_string(),
            datatype: Some(ThingDatatype::Text),
            status: Some("none".to_string()),
            status_timestamp_ms: None,
            title: Some("Only on server".to_string()),
            parent_id: None,
            trigger: TriggerUpdate::Noop,
            content: None,
        },
    )
    .unwrap();

    // Client cold start: no sync_state, but local user already created a few records.
    let mut client_doc: Vec<u8> = Vec::new();
    client_doc = apply_op(
        &client_doc,
        "device-a",
        Op::UpsertCollection {
            id: "c1".to_string(),
            title: Some("Inbox".to_string()),
            status: Some("none".to_string()),
            trigger: TriggerUpdate::Noop,
        },
    )
    .unwrap();

    for i in 0..3 {
        client_doc = apply_op(
            &client_doc,
            "device-a",
            Op::UpsertThing {
                id: format!("t_client_{i}"),
                collection_id: "c1".to_string(),
                datatype: Some(ThingDatatype::Text),
                status: Some("none".to_string()),
                status_timestamp_ms: None,
                title: Some(format!("Client Thing {i}")),
                parent_id: None,
                trigger: TriggerUpdate::Noop,
                content: None,
            },
        )
        .unwrap();
    }

    let (client_final, server_final, rounds) = converge_unary(client_doc, server_doc, 8).unwrap();

    assert!(rounds <= 4, "expected fast cold merge, got {rounds} rounds");

    let v_client = extract_view(&client_final).unwrap();
    let v_server = extract_view(&server_final).unwrap();
    assert_eq!(v_client, v_server);

    let ids: std::collections::BTreeSet<_> =
        v_client.things.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains("t_server"));
    assert!(ids.contains("t_client_0"));
    assert!(ids.contains("t_client_1"));
    assert!(ids.contains("t_client_2"));
    assert_eq!(v_client.things.len(), 4);
}

#[test]
fn scenario_3_basic_editing_ops_and_sync() {
    // Start with the same base document on both sides, then diverge with edits.
    let mut base: Vec<u8> = Vec::new();
    base = apply_op(
        &base,
        "seed",
        Op::UpsertCollection {
            id: "c1".to_string(),
            title: Some("Inbox".to_string()),
            status: Some("none".to_string()),
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
            datatype: Some(ThingDatatype::Text),
            status: Some("none".to_string()),
            status_timestamp_ms: None,
            title: Some("Title".to_string()),
            parent_id: None,
            trigger: TriggerUpdate::Noop,
            content: Some(Content::Text {
                blocks: vec![Block {
                    id: "b1".to_string(),
                    r#type: "paragraph".to_string(),
                    attrs_json: None,
                    text: Some("abcd".to_string()),
                }],
            }),
        },
    )
    .unwrap();

    // Client A edits text and inserts a new block.
    let mut doc_a = base.clone();
    doc_a = apply_op(
        &doc_a,
        "device-a",
        Op::SpliceText {
            thing_id: "t1".to_string(),
            block_id: "b1".to_string(),
            index: 2,
            delete: 1,
            insert: "Z".to_string(),
        },
    )
    .unwrap();
    doc_a = apply_op(
        &doc_a,
        "device-a",
        Op::InsertBlock {
            thing_id: "t1".to_string(),
            index: 1,
            block: Block {
                id: "b2".to_string(),
                r#type: "paragraph".to_string(),
                attrs_json: None,
                text: Some("new".to_string()),
            },
        },
    )
    .unwrap();

    // Server edits status, moves thing (no-op move here), and clears trigger.
    let mut doc_server = base.clone();
    doc_server = apply_op(
        &doc_server,
        "server",
        Op::SetThingStatus {
            id: "t1".to_string(),
            status: "done".to_string(),
            timestamp_ms: Some(1234567890),
        },
    )
    .unwrap();
    doc_server = apply_op(
        &doc_server,
        "server",
        Op::MoveThing {
            id: "t1".to_string(),
            to_collection_id: "c1".to_string(),
        },
    )
    .unwrap();
    doc_server = apply_op(
        &doc_server,
        "server",
        Op::UpsertThing {
            id: "t1".to_string(),
            collection_id: "c1".to_string(),
            datatype: None,
            status: None,
            status_timestamp_ms: None,
            title: None,
            parent_id: None,
            trigger: TriggerUpdate::Clear,
            content: None,
        },
    )
    .unwrap();

    let (a_final, s_final, rounds) = converge_unary(doc_a, doc_server, 10).unwrap();
    assert!(rounds <= 6, "expected convergence, got {rounds} rounds");

    let v = extract_view(&a_final).unwrap();
    let v2 = extract_view(&s_final).unwrap();
    assert_eq!(v, v2);

    let t1 = v.things.iter().find(|t| t.id == "t1").unwrap();
    assert_eq!(t1.status.as_storage_str(), "done");
    assert_eq!(t1.status.timestamp_ms(), Some(1234567890));

    let blocks = t1.content.as_ref().unwrap().blocks.as_ref().unwrap();
    assert_eq!(blocks.len(), 2);

    // text splice result on b1
    let b1 = blocks.iter().find(|b| b.id == "b1").unwrap();
    assert_eq!(b1.text.as_deref(), Some("abZd"));

    // inserted block exists
    assert!(blocks.iter().any(|b| b.id == "b2"));
}
