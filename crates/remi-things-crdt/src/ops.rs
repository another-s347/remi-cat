use anyhow::{Context, Result};
use automerge::transaction::Transactable;
use automerge::{AutoCommit, ObjId, ObjType, ReadDoc, ScalarValue, Value};

use crate::datatype::{ContentEntryPayload, LocationField, ThingBuiltInFieldsUpdate};
use crate::schema::Schema;
use crate::util::{
    collect_list_keys, collect_map_keys, ensure_child_map, ensure_list_key, ensure_map_key,
    ensure_map_key as ensure_root_map, get_string, get_u64, put_string, put_u64, set_doc_actor,
};
use crate::ThingDatatype;

#[derive(Debug, Clone)]
pub enum TriggerUpdate {
    Noop,
    Clear,
    Set(String),
}

#[derive(Debug, Clone)]
pub struct Block {
    pub id: String,
    pub r#type: String,
    pub attrs_json: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Content {
    Text { blocks: Vec<Block> },
    Markdown { blocks: Vec<Block> },
    Opaque { kind: String, payload_json: String },
}

#[derive(Debug, Clone)]
pub enum Op {
    /// Apply multiple upsert operations in one atomic document update.
    /// Intended for first-sync bootstrap flows where local records are replayed
    /// onto a server snapshot.
    BatchUpsert {
        ops: Vec<Op>,
    },

    UpsertCollection {
        id: String,
        title: Option<String>,
        status: Option<String>,
        trigger: TriggerUpdate,
    },
    DeleteCollection {
        id: String,
    },

    UpsertThing {
        id: String,
        collection_id: String,
        datatype: Option<ThingDatatype>,
        status: Option<String>,
        status_timestamp_ms: Option<i64>,
        title: Option<String>,
        parent_id: Option<String>,
        trigger: TriggerUpdate,
        content: Option<Content>,
    },
    DeleteThing {
        id: String,
    },
    /// Delete multiple things in one atomic operation.
    DeleteThings {
        ids: Vec<String>,
    },
    MoveThing {
        id: String,
        to_collection_id: String,
    },
    /// Move multiple things to a target collection in one atomic operation.
    MoveThings {
        ids: Vec<String>,
        to_collection_id: String,
    },
    SetThingStatus {
        id: String,
        status: String,
        timestamp_ms: Option<i64>,
    },

    InsertBlock {
        thing_id: String,
        index: usize,
        block: Block,
    },
    DeleteBlock {
        thing_id: String,
        block_id: String,
    },
    MoveBlock {
        thing_id: String,
        block_id: String,
        to_index: usize,
    },

    // CRDT text splice (Automerge Text).
    SpliceText {
        thing_id: String,
        block_id: String,
        index: usize,
        delete: usize,
        insert: String,
    },
}

pub fn apply_op(doc_bytes: &[u8], actor: &str, op: Op) -> Result<Vec<u8>> {
    let mut doc = if doc_bytes.is_empty() {
        AutoCommit::new()
    } else {
        AutoCommit::load(doc_bytes).context("Failed to load automerge doc")?
    };
    set_doc_actor(&mut doc, actor);

    // Keep epoch stable unless caller updates it explicitly; for now, default 0.
    let epoch = get_u64(&doc, &automerge::ROOT, Schema::KEY_EPOCH)?.unwrap_or(0);
    Schema::ensure_root(&mut doc, actor, epoch)?;

    fn apply_single(doc: &mut AutoCommit, actor: &str, op: Op) -> Result<()> {
        match op {
            Op::BatchUpsert { ops } => {
                for inner in ops {
                    apply_single(doc, actor, inner)?;
                }
                Ok(())
            }

            Op::UpsertCollection {
                id,
                title,
                status,
                trigger,
            } => upsert_collection(
                doc,
                actor,
                &id,
                title.as_deref(),
                status.as_deref(),
                trigger,
            ),
            Op::DeleteCollection { id } => delete_collection(doc, actor, &id),

            Op::UpsertThing {
                id,
                collection_id,
                datatype,
                status,
                status_timestamp_ms,
                title,
                parent_id,
                trigger,
                content,
            } => upsert_thing(
                doc,
                actor,
                &id,
                &collection_id,
                datatype.as_ref().map(|d| d.as_str()),
                status.as_deref(),
                status_timestamp_ms,
                title.as_deref(),
                parent_id.as_deref(),
                trigger,
                content,
            ),
            Op::DeleteThing { id } => delete_thing(doc, actor, &id),
            Op::DeleteThings { ids } => delete_things(doc, actor, &ids),
            Op::MoveThing {
                id,
                to_collection_id,
            } => move_thing(doc, actor, &id, &to_collection_id),
            Op::MoveThings {
                ids,
                to_collection_id,
            } => move_things(doc, actor, &ids, &to_collection_id),
            Op::SetThingStatus {
                id,
                status,
                timestamp_ms,
            } => set_thing_status(doc, actor, &id, &status, timestamp_ms),

            Op::InsertBlock {
                thing_id,
                index,
                block,
            } => insert_block(doc, &thing_id, index, block),
            Op::DeleteBlock { thing_id, block_id } => delete_block(doc, &thing_id, &block_id),
            Op::MoveBlock {
                thing_id,
                block_id,
                to_index,
            } => move_block(doc, &thing_id, &block_id, to_index),
            Op::SpliceText {
                thing_id,
                block_id,
                index,
                delete,
                insert,
            } => splice_text_chars(doc, &thing_id, &block_id, index, delete, &insert),
        }
    }

    apply_single(&mut doc, actor, op)?;
    Ok(doc.save())
}

fn bump_entity_clock(doc: &mut AutoCommit, entity_obj: &ObjId, actor: &str) -> Result<()> {
    let clock_obj = ensure_map_key(doc, entity_obj, "edit_clock")?;
    let prev_seq = get_u64(doc, &clock_obj, "seq")?.unwrap_or(0);
    put_string(doc, &clock_obj, "actor", actor)?;
    put_u64(doc, &clock_obj, "seq", prev_seq.saturating_add(1))?;
    Ok(())
}

fn set_tombstone(doc: &mut AutoCommit, entity_obj: &ObjId, actor: &str) -> Result<()> {
    let tomb = ensure_map_key(doc, entity_obj, "tombstone")?;
    doc.put(&tomb, "deleted", true)
        .context("Failed to set tombstone.deleted")?;
    let clock = ensure_map_key(doc, &tomb, "clock")?;
    let prev_seq = get_u64(doc, &clock, "seq")?.unwrap_or(0);
    put_string(doc, &clock, "actor", actor)?;
    put_u64(doc, &clock, "seq", prev_seq.saturating_add(1))?;
    Ok(())
}

fn clear_tombstone(doc: &mut AutoCommit, entity_obj: &ObjId) {
    let _ = doc.delete(entity_obj, "tombstone");
}

fn thing_is_live(doc: &AutoCommit, thing_obj: &ObjId) -> Result<bool> {
    Ok(!doc
        .get(thing_obj, "tombstone")?
        .and_then(|(value, tombstone_obj)| match value {
            Value::Object(ObjType::Map) => Some(tombstone_obj),
            _ => None,
        })
        .map(|tombstone_obj| {
            get_u64(doc, &tombstone_obj, "deleted")
                .ok()
                .flatten()
                .map(|deleted| deleted != 0)
                .unwrap_or_else(|| {
                    doc.get(&tombstone_obj, "deleted")
                        .ok()
                        .flatten()
                        .and_then(|(value, _)| match value {
                            Value::Scalar(sv) => match sv.as_ref() {
                                ScalarValue::Boolean(flag) => Some(*flag),
                                _ => None,
                            },
                            _ => None,
                        })
                        .unwrap_or(false)
                })
        })
        .unwrap_or(false))
}

fn find_live_thing_obj(doc: &AutoCommit, thing_id: &str) -> Result<Option<ObjId>> {
    let Some((Value::Object(ObjType::Map), things_map)) =
        doc.get(automerge::ROOT, Schema::KEY_THING_MAP)?
    else {
        return Ok(None);
    };

    for (value, thing_obj) in doc.get_all(&things_map, thing_id)? {
        if matches!(value, Value::Object(ObjType::Map)) && thing_is_live(doc, &thing_obj)? {
            return Ok(Some(thing_obj));
        }
    }

    Ok(None)
}

fn validate_parent_reference(
    doc: &AutoCommit,
    thing_id: &str,
    parent_id: Option<&str>,
) -> Result<()> {
    let Some(parent_id) = parent_id
        .map(str::trim)
        .filter(|parent_id| !parent_id.is_empty())
    else {
        return Ok(());
    };

    if parent_id == thing_id {
        anyhow::bail!("thing '{}' cannot be its own parent", thing_id);
    }

    let Some(parent_obj) = find_live_thing_obj(doc, parent_id)? else {
        anyhow::bail!(
            "parent thing '{}' must already exist in the same collection document",
            parent_id
        );
    };

    let mut cursor = Some(parent_obj);
    while let Some(current_obj) = cursor {
        let next_parent = get_string(doc, &current_obj, "parent_id")?;
        let Some(next_parent_id) = next_parent
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            break;
        };

        if next_parent_id == thing_id {
            anyhow::bail!(
                "parent assignment would create a cycle involving thing '{}'",
                thing_id
            );
        }

        cursor = find_live_thing_obj(doc, next_parent_id)?;
    }

    Ok(())
}

fn apply_trigger_update(
    doc: &mut AutoCommit,
    entity_obj: &ObjId,
    actor: &str,
    update: TriggerUpdate,
) -> Result<()> {
    match update {
        TriggerUpdate::Noop => Ok(()),
        TriggerUpdate::Clear => {
            bump_entity_clock(doc, entity_obj, actor)?;
            let trig = ensure_map_key(doc, entity_obj, "trigger")?;
            put_string(doc, &trig, "state", "none")?;
            let clock = ensure_map_key(doc, &trig, "clock")?;
            let prev_seq = get_u64(doc, &clock, "seq")?.unwrap_or(0);
            put_string(doc, &clock, "actor", actor)?;
            put_u64(doc, &clock, "seq", prev_seq.saturating_add(1))?;
            let _ = doc.delete(&trig, "uuid");
            Ok(())
        }
        TriggerUpdate::Set(uuid) => {
            bump_entity_clock(doc, entity_obj, actor)?;
            let trig = ensure_map_key(doc, entity_obj, "trigger")?;
            put_string(doc, &trig, "state", "some")?;
            put_string(doc, &trig, "uuid", uuid.trim())?;
            let clock = ensure_map_key(doc, &trig, "clock")?;
            let prev_seq = get_u64(doc, &clock, "seq")?.unwrap_or(0);
            put_string(doc, &clock, "actor", actor)?;
            put_u64(doc, &clock, "seq", prev_seq.saturating_add(1))?;
            Ok(())
        }
    }
}

fn upsert_collection(
    doc: &mut AutoCommit,
    actor: &str,
    id: &str,
    title: Option<&str>,
    status: Option<&str>,
    trigger: TriggerUpdate,
) -> Result<()> {
    let collections = ensure_root_map(doc, &automerge::ROOT, Schema::KEY_COLLECTIONS)?;
    let obj = ensure_child_map(doc, &collections, id)?;

    // If new, set defaults.
    if get_string(doc, &obj, "title")?.is_none() {
        put_string(doc, &obj, "title", "")?;
    }
    if get_string(doc, &obj, "status")?.is_none() {
        put_string(doc, &obj, "status", "active")?;
    }
    let _ = ensure_map_key(doc, &obj, "edit_clock");

    bump_entity_clock(doc, &obj, actor)?;
    clear_tombstone(doc, &obj);

    if let Some(t) = title {
        put_string(doc, &obj, "title", t)?;
    }
    if let Some(s) = status {
        put_string(doc, &obj, "status", s)?;
    }

    apply_trigger_update(doc, &obj, actor, trigger)?;

    Ok(())
}

fn delete_collection(doc: &mut AutoCommit, actor: &str, id: &str) -> Result<()> {
    // First, cascade delete all things belonging to this collection
    let view = crate::extract_view_from_doc(doc)?;
    for thing in &view.things {
        if thing.collection_id == id {
            // Skip if already deleted
            let already_deleted = thing.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false);
            if !already_deleted {
                delete_thing(doc, actor, &thing.id)?;
            }
        }
    }

    // Find max seq across ALL root "collections" maps (there can be multiple due to
    // concurrent creation by different actors) and ALL versions of this collection id.
    let max_seq = find_max_seq_for_entity(doc, Schema::KEY_COLLECTIONS, id)?;

    // Mark the collection as deleted with a higher seq so it wins.
    let collections = ensure_root_map(doc, &automerge::ROOT, Schema::KEY_COLLECTIONS)?;
    let obj = ensure_child_map(doc, &collections, id)?;
    set_entity_clock(doc, &obj, actor, max_seq.saturating_add(1))?;
    set_tombstone(doc, &obj, actor)?;

    Ok(())
}

/// Find the maximum edit_clock.seq across ALL root maps and ALL concurrent versions
/// of an entity (collection or thing) with the given id.
fn find_max_seq_for_entity(doc: &AutoCommit, root_key: &str, entity_id: &str) -> Result<u64> {
    use crate::util::collect_root_maps;

    let mut max_seq: u64 = 0;
    let root_maps = collect_root_maps(doc, root_key)?;

    for root_map in root_maps {
        if let Ok(all) = doc.get_all(&root_map, entity_id) {
            for (val, obj_id) in all {
                if matches!(val, Value::Object(ObjType::Map)) {
                    if let Some((Value::Object(ObjType::Map), clock_obj)) =
                        doc.get(&obj_id, "edit_clock")?
                    {
                        let seq = get_u64(doc, &clock_obj, "seq")?.unwrap_or(0);
                        max_seq = max_seq.max(seq);
                    }
                }
            }
        }
    }

    Ok(max_seq)
}

/// Set the edit_clock to a specific seq value (instead of incrementing).
fn set_entity_clock(doc: &mut AutoCommit, entity_obj: &ObjId, actor: &str, seq: u64) -> Result<()> {
    let clock_obj = ensure_map_key(doc, entity_obj, "edit_clock")?;
    put_string(doc, &clock_obj, "actor", actor)?;
    put_u64(doc, &clock_obj, "seq", seq)?;
    Ok(())
}

fn ensure_thing_obj(doc: &mut AutoCommit, thing_id: &str) -> Result<(ObjId, ObjId)> {
    let things = ensure_root_map(doc, &automerge::ROOT, Schema::KEY_THINGS)?;
    let obj = ensure_child_map(doc, &things, thing_id)?;
    Ok((things, obj))
}

fn upsert_thing(
    doc: &mut AutoCommit,
    actor: &str,
    id: &str,
    collection_id: &str,
    datatype: Option<&str>,
    status: Option<&str>,
    status_timestamp_ms: Option<i64>,
    title: Option<&str>,
    parent_id: Option<&str>,
    trigger: TriggerUpdate,
    content: Option<Content>,
) -> Result<()> {
    let (_things, obj) = ensure_thing_obj(doc, id)?;

    if get_string(doc, &obj, "collection_id")?.is_none() {
        put_string(doc, &obj, "collection_id", collection_id)?;
    }
    if get_string(doc, &obj, "datatype")?.is_none() {
        put_string(doc, &obj, "datatype", ThingDatatype::Markdown.as_str())?;
    }
    if get_string(doc, &obj, "status")?.is_none() {
        put_string(doc, &obj, "status", "none")?;
    }
    let _ = ensure_map_key(doc, &obj, "edit_clock");

    bump_entity_clock(doc, &obj, actor)?;
    clear_tombstone(doc, &obj);

    put_string(doc, &obj, "collection_id", collection_id)?;
    if let Some(dt) = datatype {
        put_string(doc, &obj, "datatype", dt)?;
    }
    if let Some(st) = status {
        put_string(doc, &obj, "status", st)?;

        // Handle timestamp
        if let Some(ts) = status_timestamp_ms {
            put_u64(doc, &obj, "status_timestamp_ms", ts as u64)?;
        } else if status_needs_timestamp(st) {
            // Auto-set timestamp for statuses that need it
            let now = chrono::Utc::now().timestamp_millis();
            put_u64(doc, &obj, "status_timestamp_ms", now as u64)?;
        } else if st == "none" {
            // Clear timestamp for "none" status
            let _ = doc.delete(&obj, "status_timestamp_ms");
        }
    }
    if let Some(t) = title {
        put_string(doc, &obj, "title", t)?;
    }

    // Tri-state: None = no change, Some("") = clear, Some(uuid) = set
    match parent_id {
        Some(pid) => {
            let pid = pid.trim();
            if pid.is_empty() {
                let _ = doc.delete(&obj, "parent_id");
            } else {
                put_string(doc, &obj, "parent_id", pid)?;
            }
        }
        None => {} // no change
    }

    apply_trigger_update(doc, &obj, actor, trigger)?;

    if let Some(content) = content {
        set_content(doc, &obj, content)?;
    }

    Ok(())
}

fn delete_thing(doc: &mut AutoCommit, actor: &str, id: &str) -> Result<()> {
    // First, cascade delete all child things (things with parent_id == this thing)
    let view = crate::extract_view_from_doc(doc)?;
    for child in &view.things {
        if child.parent_id.as_deref() == Some(id) {
            let already_deleted = child.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false);
            if !already_deleted {
                // Recursive call to handle nested children
                delete_thing(doc, actor, &child.id)?;
            }
        }
    }

    // Find max seq across ALL root "things" maps and all versions.
    let max_seq = find_max_seq_for_entity(doc, Schema::KEY_THINGS, id)?;

    // Mark the thing as deleted with a higher seq so it wins.
    let things = ensure_root_map(doc, &automerge::ROOT, Schema::KEY_THINGS)?;
    let obj = ensure_child_map(doc, &things, id)?;
    set_entity_clock(doc, &obj, actor, max_seq.saturating_add(1))?;
    set_tombstone(doc, &obj, actor)?;
    Ok(())
}

fn delete_things(doc: &mut AutoCommit, actor: &str, ids: &[String]) -> Result<()> {
    for id in ids {
        delete_thing(doc, actor, id)?;
    }
    Ok(())
}

fn move_thing(doc: &mut AutoCommit, actor: &str, id: &str, to_collection_id: &str) -> Result<()> {
    let (_things, obj) = ensure_thing_obj(doc, id)?;
    bump_entity_clock(doc, &obj, actor)?;
    put_string(doc, &obj, "collection_id", to_collection_id)?;
    Ok(())
}

fn move_things(
    doc: &mut AutoCommit,
    actor: &str,
    ids: &[String],
    to_collection_id: &str,
) -> Result<()> {
    for id in ids {
        move_thing(doc, actor, id, to_collection_id)?;
    }
    Ok(())
}

/// Check if a status string requires a timestamp
fn status_needs_timestamp(status: &str) -> bool {
    matches!(status, "in-progress" | "stalled" | "done")
}

fn set_thing_status(
    doc: &mut AutoCommit,
    actor: &str,
    id: &str,
    status: &str,
    timestamp_ms: Option<i64>,
) -> Result<()> {
    let (_things, obj) = ensure_thing_obj(doc, id)?;
    bump_entity_clock(doc, &obj, actor)?;
    put_string(doc, &obj, "status", status)?;

    // Store timestamp if provided
    if let Some(ts) = timestamp_ms {
        put_u64(doc, &obj, "status_timestamp_ms", ts as u64)?;
    } else if status_needs_timestamp(status) {
        // For statuses that need timestamps, use current time
        let now = chrono::Utc::now().timestamp_millis();
        put_u64(doc, &obj, "status_timestamp_ms", now as u64)?;
    } else {
        // For "none" or other statuses, remove the timestamp
        let _ = doc.delete(&obj, "status_timestamp_ms");
    }

    Ok(())
}

fn set_content(doc: &mut AutoCommit, thing_obj: &ObjId, content: Content) -> Result<()> {
    let content_obj = ensure_map_key(doc, thing_obj, "content")?;
    match content {
        Content::Text { blocks } => {
            put_string(doc, &content_obj, "kind", "text")?;
            let blocks_list = ensure_list_key(doc, &content_obj, "blocks")?;
            // replace blocks list by clearing and re-inserting
            let len = doc.length(&blocks_list);
            for i in (0..len).rev() {
                let _ = doc.delete(&blocks_list, i);
            }
            for (i, b) in blocks.into_iter().enumerate() {
                let block_obj = doc
                    .insert_object(&blocks_list, i, ObjType::Map)
                    .context("Failed to insert block object")?;
                put_string(doc, &block_obj, "id", &b.id)?;
                put_string(doc, &block_obj, "type", &b.r#type)?;
                if let Some(attrs) = b.attrs_json {
                    put_string(doc, &block_obj, "attrs", &attrs)?;
                }
                if let Some(text) = b.text {
                    set_block_text_chars(doc, &block_obj, &text)?;
                }
            }
        }
        Content::Markdown { blocks } => {
            put_string(doc, &content_obj, "kind", "markdown")?;
            let blocks_list = ensure_list_key(doc, &content_obj, "blocks")?;
            let len = doc.length(&blocks_list);
            for i in (0..len).rev() {
                let _ = doc.delete(&blocks_list, i);
            }
            for (i, b) in blocks.into_iter().enumerate() {
                let block_obj = doc
                    .insert_object(&blocks_list, i, ObjType::Map)
                    .context("Failed to insert block object")?;
                put_string(doc, &block_obj, "id", &b.id)?;
                put_string(doc, &block_obj, "type", &b.r#type)?;
                if let Some(attrs) = b.attrs_json {
                    put_string(doc, &block_obj, "attrs", &attrs)?;
                }
                if let Some(text) = b.text {
                    set_block_text_chars(doc, &block_obj, &text)?;
                }
            }
        }
        Content::Opaque { kind, payload_json } => {
            put_string(doc, &content_obj, "kind", &kind)?;
            put_string(doc, &content_obj, "payload", &payload_json)?;
        }
    }

    Ok(())
}

fn blocks_list(doc: &mut AutoCommit, thing_obj: &ObjId) -> Result<ObjId> {
    let content_obj = ensure_map_key(doc, thing_obj, "content")?;
    ensure_list_key(doc, &content_obj, "blocks")
}

fn find_block_obj(
    doc: &AutoCommit,
    blocks: &ObjId,
    block_id: &str,
) -> Result<Option<(usize, ObjId)>> {
    for idx in 0..doc.length(blocks) {
        let Some((Value::Object(ObjType::Map), obj)) = doc.get(blocks, idx)? else {
            continue;
        };
        if let Some(id) = get_string(doc, &obj, "id")? {
            if id == block_id {
                return Ok(Some((idx, obj)));
            }
        }
    }
    Ok(None)
}

fn insert_block(doc: &mut AutoCommit, thing_id: &str, index: usize, block: Block) -> Result<()> {
    let (_things, thing_obj) = ensure_thing_obj(doc, thing_id)?;
    let blocks = blocks_list(doc, &thing_obj)?;
    let idx = index.min(doc.length(&blocks));
    let block_obj = doc
        .insert_object(&blocks, idx, ObjType::Map)
        .context("Failed to insert block object")?;
    put_string(doc, &block_obj, "id", &block.id)?;
    put_string(doc, &block_obj, "type", &block.r#type)?;
    if let Some(attrs) = block.attrs_json {
        put_string(doc, &block_obj, "attrs", &attrs)?;
    }
    if let Some(text) = block.text {
        set_block_text_chars(doc, &block_obj, &text)?;
    }
    Ok(())
}

fn delete_block(doc: &mut AutoCommit, thing_id: &str, block_id: &str) -> Result<()> {
    let (_things, thing_obj) = ensure_thing_obj(doc, thing_id)?;
    let blocks = blocks_list(doc, &thing_obj)?;
    if let Some((idx, _obj)) = find_block_obj(doc, &blocks, block_id)? {
        doc.delete(&blocks, idx).context("Failed to delete block")?;
    }
    Ok(())
}

fn move_block(doc: &mut AutoCommit, thing_id: &str, block_id: &str, to_index: usize) -> Result<()> {
    let (_things, thing_obj) = ensure_thing_obj(doc, thing_id)?;
    let blocks = blocks_list(doc, &thing_obj)?;
    let Some((_from_idx, _obj)) = find_block_obj(doc, &blocks, block_id)? else {
        return Ok(());
    };

    // Copy-map: remove then reinsert a shallow clone.
    let to_index = to_index.min(doc.length(&blocks).saturating_sub(1));
    let snapshot = doc.save();
    // Reloading to avoid dangling ObjId after delete is safer.
    let mut tmp = AutoCommit::load(&snapshot).context("Failed to reload doc")?;
    set_doc_actor(&mut tmp, "move-block");

    let (_t2, thing_obj2) = ensure_thing_obj(&mut tmp, thing_id)?;
    let blocks2 = blocks_list(&mut tmp, &thing_obj2)?;
    let Some((from_idx2, obj2)) = find_block_obj(&tmp, &blocks2, block_id)? else {
        *doc = tmp;
        return Ok(());
    };

    let id = get_string(&tmp, &obj2, "id")?.unwrap_or_default();
    let typ = get_string(&tmp, &obj2, "type")?.unwrap_or_else(|| "paragraph".to_string());
    let attrs = get_string(&tmp, &obj2, "attrs")?;
    let text = read_block_text_chars(&tmp, &obj2)?;

    tmp.delete(&blocks2, from_idx2).ok();

    let insert_at = if from_idx2 < to_index {
        to_index.saturating_sub(1)
    } else {
        to_index
    };
    let new_obj = tmp
        .insert_object(&blocks2, insert_at, ObjType::Map)
        .context("Failed to reinsert block")?;
    put_string(&mut tmp, &new_obj, "id", &id)?;
    put_string(&mut tmp, &new_obj, "type", &typ)?;
    if let Some(attrs) = attrs {
        put_string(&mut tmp, &new_obj, "attrs", &attrs)?;
    }
    if let Some(text) = text {
        set_block_text_chars(&mut tmp, &new_obj, &text)?;
    }

    *doc = tmp;
    Ok(())
}

fn ensure_block_text_obj(doc: &mut AutoCommit, block_obj: &ObjId) -> Result<ObjId> {
    if let Some((Value::Object(ObjType::Text), id)) = doc.get(block_obj, "text")? {
        return Ok(id);
    }

    // Migrate legacy encodings (list/scalar) into an Automerge Text object.
    let existing = read_block_text_chars(doc, block_obj)?.unwrap_or_default();
    let _ = doc.delete(block_obj, "text");
    let text_obj = doc
        .put_object(block_obj, "text", ObjType::Text)
        .context("Failed to create block.text Text")?;
    if !existing.is_empty() {
        doc.splice_text(&text_obj, 0, 0, &existing)
            .context("Failed to migrate legacy text into Text")?;
    }
    Ok(text_obj)
}

fn set_block_text_chars(doc: &mut AutoCommit, block_obj: &ObjId, text: &str) -> Result<()> {
    let text_obj = ensure_block_text_obj(doc, block_obj)?;
    let len = doc.length(&text_obj);
    let del = isize::try_from(len).unwrap_or(isize::MAX);
    doc.splice_text(&text_obj, 0, del, text)
        .context("Failed to replace block text")?;
    Ok(())
}

fn read_block_text_chars(doc: &AutoCommit, block_obj: &ObjId) -> Result<Option<String>> {
    let Some((val, obj)) = doc.get(block_obj, "text")? else {
        return Ok(None);
    };

    match val {
        Value::Object(ObjType::Text) => {
            let text_obj = obj;
            let s = doc.text(&text_obj).context("Failed to read Text")?;
            Ok(Some(s))
        }
        Value::Scalar(sv) => match sv.as_ref() {
            automerge::ScalarValue::Str(s) => Ok(Some(s.to_string())),
            automerge::ScalarValue::Bytes(b) => Ok(Some(String::from_utf8_lossy(b).to_string())),
            _ => Ok(None),
        },
        Value::Object(ObjType::List) => {
            let list = obj;
            let mut out = String::new();
            for i in 0..doc.length(&list) {
                match doc.get(&list, i)? {
                    Some((Value::Scalar(sv), _)) => match sv.as_ref() {
                        automerge::ScalarValue::Str(s) => out.push_str(s),
                        automerge::ScalarValue::Bytes(b) => {
                            out.push_str(&String::from_utf8_lossy(b))
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            Ok(Some(out))
        }
        _ => Ok(None),
    }
}

fn splice_text_chars(
    doc: &mut AutoCommit,
    thing_id: &str,
    block_id: &str,
    index: usize,
    delete: usize,
    insert: &str,
) -> Result<()> {
    let (_things, thing_obj) = ensure_thing_obj(doc, thing_id)?;
    let blocks = blocks_list(doc, &thing_obj)?;
    let Some((_idx, block_obj)) = find_block_obj(doc, &blocks, block_id)? else {
        anyhow::bail!("Block '{}' not found in thing '{}'", block_id, thing_id);
    };

    // Fast path for full overwrite: callers in higher layers use `delete = usize::MAX` as a
    // sentinel meaning "replace everything". Implement this as a scalar replacement to avoid
    // expensive character-level splice operations (and legacy -> Text migrations) on mobile.
    if index == 0 && delete == usize::MAX {
        let _ = doc.delete(&block_obj, "text");
        put_string(doc, &block_obj, "text", insert)?;
        return Ok(());
    }

    let text_obj = ensure_block_text_obj(doc, &block_obj)?;
    let len = doc.length(&text_obj);
    let start = index.min(len);
    let max_delete = len.saturating_sub(start);
    let delete = delete.min(max_delete);
    let del = isize::try_from(delete).unwrap_or(isize::MAX);
    doc.splice_text(&text_obj, start, del, insert)
        .context("Failed to splice text")?;

    Ok(())
}

// ============================================================================
// V3 Multi-Document Operations
// ============================================================================

/// Operations for Root document (CrdtDataType::Root)
#[derive(Debug, Clone)]
pub enum RootOp {
    /// Add a collection UUID to the root document
    AddCollection { uuid: String },
    /// Remove a collection UUID from the root document
    RemoveCollection { uuid: String },
    /// Increment the epoch (used for major sync events)
    IncrementEpoch,
}

/// Operations for Collection document (CrdtDataType::Collection)
#[derive(Debug, Clone)]
pub enum CollectionOp {
    /// Update collection metadata
    UpdateMeta {
        title: Option<String>,
        status: Option<String>,
        trigger: TriggerUpdate,
        attrs_json: Option<String>,
    },
    /// Delete the collection (set tombstone)
    Delete,
    /// Upsert a thing's metadata within this collection
    UpsertThingMeta {
        thing_id: String,
        datatype: Option<ThingDatatype>,
        status: Option<String>,
        status_timestamp_ms: Option<i64>,
        title: Option<String>,
        parent_id: Option<String>,
        trigger: TriggerUpdate,
        built_in: Option<ThingBuiltInFieldsUpdate>,
        attrs_json: Option<String>,
    },
    /// Delete a thing (set tombstone on thing metadata)
    DeleteThing { thing_id: String },
    /// Batch delete multiple things
    DeleteThings { thing_ids: Vec<String> },
    /// Move a thing from another collection into this one
    MoveThing {
        thing_id: String,
        from_collection_uuid: String,
    },
}

/// Operations for ThingMarkdown document (CrdtDataType::ThingMarkdown)
#[derive(Debug, Clone)]
pub enum ThingMarkdownOp {
    /// Set the full content
    SetContent { content: Content },
    /// Insert a block at index
    InsertBlock { index: usize, block: Block },
    /// Delete a block by id
    DeleteBlock { block_id: String },
    /// Move a block to a new index
    MoveBlock { block_id: String, to_index: usize },
    /// Splice text within a block
    SpliceText {
        block_id: String,
        index: usize,
        delete: usize,
        insert: String,
    },
}

/// Apply a RootOp to a Root document
pub fn apply_root_op(doc_bytes: &[u8], actor: &str, op: RootOp) -> Result<Vec<u8>> {
    let mut doc = if doc_bytes.is_empty() {
        let init_bytes = Schema::init_root_doc(actor)?;
        AutoCommit::load(&init_bytes).context("Failed to load init root doc")?
    } else {
        AutoCommit::load(doc_bytes).context("Failed to load root doc")?
    };
    set_doc_actor(&mut doc, actor);

    match op {
        RootOp::AddCollection { uuid } => {
            let list_obj = if let Some((Value::Object(ObjType::List), obj)) =
                doc.get(automerge::ROOT, Schema::KEY_COLLECTION_UUIDS)?
            {
                obj
            } else {
                doc.put_object(automerge::ROOT, Schema::KEY_COLLECTION_UUIDS, ObjType::List)
                    .context("Failed to create collection_uuids list")?
            };

            // Check if already present
            let mut found = false;
            for i in 0..doc.length(&list_obj) {
                if let Some((Value::Scalar(sv), _)) = doc.get(&list_obj, i)? {
                    if let ScalarValue::Str(s) = sv.as_ref() {
                        if s == &uuid {
                            found = true;
                            break;
                        }
                    }
                }
            }
            if !found {
                doc.insert(&list_obj, doc.length(&list_obj), uuid)
                    .context("Failed to add collection uuid")?;
            }
        }
        RootOp::RemoveCollection { uuid } => {
            if let Some((Value::Object(ObjType::List), list_obj)) =
                doc.get(automerge::ROOT, Schema::KEY_COLLECTION_UUIDS)?
            {
                for i in (0..doc.length(&list_obj)).rev() {
                    if let Some((Value::Scalar(sv), _)) = doc.get(&list_obj, i)? {
                        if let ScalarValue::Str(s) = sv.as_ref() {
                            if s == &uuid {
                                doc.delete(&list_obj, i)
                                    .context("Failed to remove collection uuid")?;
                                break;
                            }
                        }
                    }
                }
            }
        }
        RootOp::IncrementEpoch => {
            let current = get_u64(&doc, &automerge::ROOT, Schema::KEY_EPOCH)?.unwrap_or(0);
            put_u64(&mut doc, &automerge::ROOT, Schema::KEY_EPOCH, current + 1)?;
        }
    }

    Ok(doc.save())
}

/// Apply a CollectionOp to a Collection document
pub fn apply_collection_op(
    doc_bytes: &[u8],
    actor: &str,
    collection_uuid: &str,
    op: CollectionOp,
) -> Result<Vec<u8>> {
    let mut doc = if doc_bytes.is_empty() {
        let init_bytes = Schema::init_collection_doc(actor, collection_uuid)?;
        AutoCommit::load(&init_bytes).context("Failed to load init collection doc")?
    } else {
        AutoCommit::load(doc_bytes).context("Failed to load collection doc")?
    };
    set_doc_actor(&mut doc, actor);

    match op {
        CollectionOp::UpdateMeta {
            title,
            status,
            trigger,
            attrs_json,
        } => {
            let meta_obj = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_META)?;
            bump_entity_clock(&mut doc, &meta_obj, actor)?;
            clear_tombstone(&mut doc, &meta_obj);

            if let Some(t) = title {
                put_string(&mut doc, &meta_obj, "title", &t)?;
            }
            if let Some(s) = status {
                put_string(&mut doc, &meta_obj, "status", &s)?;
            }
            if let Some(attrs) = attrs_json {
                put_string(&mut doc, &meta_obj, "attrs", &attrs)?;
            }
            apply_trigger_update(&mut doc, &meta_obj, actor, trigger)?;
        }
        CollectionOp::Delete => {
            let meta_obj = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_META)?;
            set_tombstone(&mut doc, &meta_obj, actor)?;
        }
        CollectionOp::UpsertThingMeta {
            thing_id,
            datatype,
            status,
            status_timestamp_ms,
            title,
            parent_id,
            trigger,
            built_in,
            attrs_json,
        } => {
            validate_parent_reference(&doc, &thing_id, parent_id.as_deref())?;

            let things_map = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_THING_MAP)?;
            let thing_obj = ensure_child_map(&mut doc, &things_map, &thing_id)?;

            // Set defaults if new
            if get_string(&doc, &thing_obj, "datatype")?.is_none() {
                put_string(
                    &mut doc,
                    &thing_obj,
                    "datatype",
                    ThingDatatype::Markdown.as_str(),
                )?;
            }
            if get_string(&doc, &thing_obj, "status")?.is_none() {
                put_string(&mut doc, &thing_obj, "status", "none")?;
            }
            let _ = ensure_map_key(&mut doc, &thing_obj, "edit_clock");
            let built_in_obj = ensure_map_key(&mut doc, &thing_obj, Schema::KEY_BUILT_IN)?;
            let _ = ensure_list_key(&mut doc, &built_in_obj, "content_entries")?;

            bump_entity_clock(&mut doc, &thing_obj, actor)?;
            clear_tombstone(&mut doc, &thing_obj);

            if let Some(dt) = datatype {
                put_string(&mut doc, &thing_obj, "datatype", dt.as_str())?;
            }
            if let Some(st) = status {
                put_string(&mut doc, &thing_obj, "status", &st)?;

                if let Some(ts) = status_timestamp_ms {
                    put_u64(&mut doc, &thing_obj, "status_timestamp_ms", ts as u64)?;
                } else if status_needs_timestamp(&st) {
                    let now = chrono::Utc::now().timestamp_millis();
                    put_u64(&mut doc, &thing_obj, "status_timestamp_ms", now as u64)?;
                } else if st == "none" {
                    let _ = doc.delete(&thing_obj, "status_timestamp_ms");
                }
            }
            if let Some(t) = title {
                put_string(&mut doc, &thing_obj, "title", &t)?;
            }
            // Tri-state: None = no change, Some("") = clear, Some(uuid) = set
            match parent_id.as_deref() {
                Some("") => {
                    let _ = doc.delete(&thing_obj, "parent_id");
                }
                Some(pid) => put_string(&mut doc, &thing_obj, "parent_id", pid.trim())?,
                None => {} // no change
            }
            if let Some(attrs) = attrs_json {
                put_string(&mut doc, &thing_obj, "attrs", &attrs)?;
            }
            apply_trigger_update(&mut doc, &thing_obj, actor, trigger)?;

            // Handle built_in fields
            if let Some(bi) = built_in {
                apply_built_in_fields(&mut doc, &thing_obj, bi)?;
            }
        }
        CollectionOp::DeleteThing { thing_id } => {
            let things_map = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_THING_MAP)?;
            let thing_obj = ensure_child_map(&mut doc, &things_map, &thing_id)?;
            set_tombstone(&mut doc, &thing_obj, actor)?;
        }
        CollectionOp::DeleteThings { thing_ids } => {
            let things_map = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_THING_MAP)?;
            for thing_id in thing_ids {
                let thing_obj = ensure_child_map(&mut doc, &things_map, &thing_id)?;
                set_tombstone(&mut doc, &thing_obj, actor)?;
            }
        }
        CollectionOp::MoveThing {
            thing_id,
            from_collection_uuid: _,
        } => {
            // In v3, moving a thing means:
            // 1. The source collection doc marks the thing as deleted/moved
            // 2. The target collection doc upserts the thing metadata
            // This op handles the target side - just ensure the thing exists
            let things_map = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_THING_MAP)?;
            let thing_obj = ensure_child_map(&mut doc, &things_map, &thing_id)?;
            bump_entity_clock(&mut doc, &thing_obj, actor)?;
        }
    }

    Ok(doc.save())
}

/// Apply ThingBuiltInFieldsUpdate to a thing object (V3 multi-value content entries)
fn apply_built_in_fields(
    doc: &mut AutoCommit,
    thing_obj: &ObjId,
    update: ThingBuiltInFieldsUpdate,
) -> Result<()> {
    let mut built_in_objs = collect_map_keys(doc, thing_obj, Schema::KEY_BUILT_IN)?;
    let built_in_obj = if let Some(existing) = built_in_objs.first().cloned() {
        existing
    } else {
        let created = ensure_map_key(doc, thing_obj, Schema::KEY_BUILT_IN)?;
        built_in_objs.push(created.clone());
        created
    };

    let mut entries_lists = Vec::new();
    for built_in_obj in &built_in_objs {
        entries_lists.extend(collect_list_keys(doc, built_in_obj, "content_entries")?);
    }
    let primary_entries_list = if let Some(existing) = entries_lists.first().cloned() {
        existing
    } else {
        let created = ensure_list_key(doc, &built_in_obj, "content_entries")?;
        entries_lists.push(created.clone());
        created
    };

    // Delete entries by ID
    for entry_id in &update.delete_entry_ids {
        for entries_list in &entries_lists {
            let len = doc.length(entries_list);
            for i in (0..len).rev() {
                if let Some((Value::Object(ObjType::Map), entry_obj)) = doc.get(entries_list, i)? {
                    if let Some(id) = get_string(doc, &entry_obj, "id")? {
                        if &id == entry_id {
                            doc.delete(entries_list, i)
                                .context("Failed to delete entry")?;
                        }
                    }
                }
            }
        }
    }

    // Update existing entries
    for entry_update in &update.update_entries {
        for entries_list in &entries_lists {
            let len = doc.length(entries_list);
            for i in 0..len {
                if let Some((Value::Object(ObjType::Map), entry_obj)) = doc.get(entries_list, i)? {
                    if let Some(id) = get_string(doc, &entry_obj, "id")? {
                        if id == entry_update.id {
                            if let Some(title_opt) = &entry_update.title {
                                match title_opt {
                                    Some(title) => put_string(doc, &entry_obj, "title", title)?,
                                    None => {
                                        let _ = doc.delete(&entry_obj, "title");
                                    }
                                }
                            }
                            if let Some(order) = entry_update.order {
                                doc.put(&entry_obj, "order", order)
                                    .context("Failed to set order")?;
                            }
                            if let Some(payload) = &entry_update.payload {
                                write_content_entry_payload(doc, &entry_obj, payload)?;
                            }
                        }
                    }
                }
            }
        }
    }

    // Add new entries
    for entry in &update.add_entries {
        let idx = doc.length(&primary_entries_list);
        let entry_obj = doc
            .insert_object(&primary_entries_list, idx, ObjType::Map)
            .context("Failed to insert entry")?;

        put_string(doc, &entry_obj, "id", &entry.id)?;
        if let Some(title) = &entry.title {
            put_string(doc, &entry_obj, "title", title)?;
        }
        doc.put(&entry_obj, "order", entry.order)
            .context("Failed to set order")?;

        write_content_entry_payload(doc, &entry_obj, &entry.payload)?;
    }

    // Handle extra: Some(Some(json)) = set, Some(None) = clear, None = unchanged
    if let Some(extra_opt) = update.extra {
        match extra_opt {
            Some(extra_val) => {
                let json_str =
                    serde_json::to_string(&extra_val).context("Failed to serialize extra")?;
                put_string(doc, &built_in_obj, "extra", &json_str)?;
            }
            None => {
                let _ = doc.delete(&built_in_obj, "extra");
            }
        }
    }

    Ok(())
}

/// Write content entry payload to an entry object
fn write_content_entry_payload(
    doc: &mut AutoCommit,
    entry_obj: &ObjId,
    payload: &ContentEntryPayload,
) -> Result<()> {
    // First, clear any existing payload fields
    let _ = doc.delete(entry_obj, "payload");

    let payload_obj = ensure_map_key(doc, entry_obj, "payload")?;

    match payload {
        ContentEntryPayload::Location(loc) => {
            put_string(doc, &payload_obj, "type", "location")?;
            match loc {
                LocationField::Coordinate {
                    lat,
                    lng,
                    coord_system,
                    source_name,
                } => {
                    put_string(doc, &payload_obj, "loc_type", "coordinate")?;
                    doc.put(&payload_obj, "lat", *lat)
                        .context("Failed to set lat")?;
                    doc.put(&payload_obj, "lng", *lng)
                        .context("Failed to set lng")?;
                    put_string(doc, &payload_obj, "coord_system", coord_system)?;
                    if let Some(name) = source_name {
                        put_string(doc, &payload_obj, "source_name", name)?;
                    }
                }
                LocationField::Fuzzy { name, place_type } => {
                    put_string(doc, &payload_obj, "loc_type", "fuzzy")?;
                    put_string(doc, &payload_obj, "name", name)?;
                    put_string(doc, &payload_obj, "place_type", place_type)?;
                }
            }
        }
        ContentEntryPayload::Markdown { doc_uuid } => {
            put_string(doc, &payload_obj, "type", "markdown")?;
            put_string(doc, &payload_obj, "doc_uuid", doc_uuid)?;
        }
        ContentEntryPayload::JsonObject(field) => {
            put_string(doc, &payload_obj, "type", "json_object")?;
            put_string(doc, &payload_obj, "data_doc_uuid", &field.data_doc_uuid)?;
            if let Some(schema_doc_uuid) = &field.schema_doc_uuid {
                put_string(doc, &payload_obj, "schema_doc_uuid", schema_doc_uuid)?;
            }
        }
        ContentEntryPayload::Date(date) => {
            put_string(doc, &payload_obj, "type", "date")?;
            put_u64(doc, &payload_obj, "timestamp_ms", date.timestamp_ms as u64)?;
            doc.put(&payload_obj, "has_time", date.has_time)
                .context("Failed to set has_time")?;
            if let Some(tz) = &date.timezone {
                put_string(doc, &payload_obj, "timezone", tz)?;
            }
        }
        ContentEntryPayload::Image(image) => {
            put_string(doc, &payload_obj, "type", "image")?;
            put_string(doc, &payload_obj, "uri", &image.uri)?;
            if let Some(caption) = &image.caption {
                put_string(doc, &payload_obj, "caption", caption)?;
            }
            if let Some(width) = image.width {
                put_u64(doc, &payload_obj, "width", width as u64)?;
            }
            if let Some(height) = image.height {
                put_u64(doc, &payload_obj, "height", height as u64)?;
            }
            if let Some(size_bytes) = image.size_bytes {
                put_u64(doc, &payload_obj, "size_bytes", size_bytes)?;
            }
            if let Some(device_id) = &image.device_id {
                put_string(doc, &payload_obj, "device_id", device_id)?;
            }
        }
        ContentEntryPayload::Url(url_field) => {
            put_string(doc, &payload_obj, "type", "url")?;
            put_string(doc, &payload_obj, "url", &url_field.url)?;
            if let Some(title) = &url_field.title {
                put_string(doc, &payload_obj, "title", title)?;
            }
            if let Some(description) = &url_field.description {
                put_string(doc, &payload_obj, "description", description)?;
            }
            if let Some(image_url) = &url_field.image_url {
                put_string(doc, &payload_obj, "image_url", image_url)?;
            }
            if let Some(favicon_url) = &url_field.favicon_url {
                put_string(doc, &payload_obj, "favicon_url", favicon_url)?;
            }
            if let Some(site_name) = &url_field.site_name {
                put_string(doc, &payload_obj, "site_name", site_name)?;
            }
            doc.put(&payload_obj, "resolved", url_field.resolved)
                .context("Failed to set resolved")?;
        }
        ContentEntryPayload::Custom { content_type, data } => {
            put_string(doc, &payload_obj, "type", content_type)?;
            let json_str =
                serde_json::to_string(data).context("Failed to serialize custom payload")?;
            put_string(doc, &payload_obj, "data", &json_str)?;
        }
    }

    Ok(())
}

/// Apply a ThingMarkdownOp to a ThingMarkdown document
pub fn apply_thing_markdown_op(
    doc_bytes: &[u8],
    actor: &str,
    thing_uuid: &str,
    op: ThingMarkdownOp,
) -> Result<Vec<u8>> {
    let mut doc = if doc_bytes.is_empty() {
        let init_bytes = Schema::init_thing_markdown_doc(actor, thing_uuid)?;
        AutoCommit::load(&init_bytes).context("Failed to load init thing markdown doc")?
    } else {
        AutoCommit::load(doc_bytes).context("Failed to load thing markdown doc")?
    };
    set_doc_actor(&mut doc, actor);

    match op {
        ThingMarkdownOp::SetContent { content } => {
            let content_obj = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_CONTENT)?;
            set_content_v3(&mut doc, &content_obj, content)?;
        }
        ThingMarkdownOp::InsertBlock { index, block } => {
            let content_obj = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_CONTENT)?;
            let blocks = ensure_list_key(&mut doc, &content_obj, "blocks")?;
            let idx = index.min(doc.length(&blocks));
            let block_obj = doc
                .insert_object(&blocks, idx, ObjType::Map)
                .context("Failed to insert block")?;
            put_string(&mut doc, &block_obj, "id", &block.id)?;
            put_string(&mut doc, &block_obj, "type", &block.r#type)?;
            if let Some(attrs) = block.attrs_json {
                put_string(&mut doc, &block_obj, "attrs", &attrs)?;
            }
            if let Some(text) = block.text {
                set_block_text_chars(&mut doc, &block_obj, &text)?;
            }
        }
        ThingMarkdownOp::DeleteBlock { block_id } => {
            let content_obj = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_CONTENT)?;
            let blocks = ensure_list_key(&mut doc, &content_obj, "blocks")?;
            if let Some((idx, _)) = find_block_obj(&doc, &blocks, &block_id)? {
                doc.delete(&blocks, idx).context("Failed to delete block")?;
            }
        }
        ThingMarkdownOp::MoveBlock { block_id, to_index } => {
            let content_obj = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_CONTENT)?;
            let blocks = ensure_list_key(&mut doc, &content_obj, "blocks")?;
            if let Some((from_idx, obj)) = find_block_obj(&doc, &blocks, &block_id)? {
                // Copy block data
                let id = get_string(&doc, &obj, "id")?.unwrap_or_default();
                let typ =
                    get_string(&doc, &obj, "type")?.unwrap_or_else(|| "paragraph".to_string());
                let attrs = get_string(&doc, &obj, "attrs")?;
                let text = read_block_text_chars(&doc, &obj)?;

                // Delete and reinsert
                doc.delete(&blocks, from_idx).ok();
                let insert_at = if from_idx < to_index {
                    to_index.saturating_sub(1)
                } else {
                    to_index
                };
                let new_obj = doc
                    .insert_object(&blocks, insert_at, ObjType::Map)
                    .context("Failed to reinsert block")?;
                put_string(&mut doc, &new_obj, "id", &id)?;
                put_string(&mut doc, &new_obj, "type", &typ)?;
                if let Some(a) = attrs {
                    put_string(&mut doc, &new_obj, "attrs", &a)?;
                }
                if let Some(t) = text {
                    set_block_text_chars(&mut doc, &new_obj, &t)?;
                }
            }
        }
        ThingMarkdownOp::SpliceText {
            block_id,
            index,
            delete,
            insert,
        } => {
            let content_obj = ensure_map_key(&mut doc, &automerge::ROOT, Schema::KEY_CONTENT)?;
            let blocks = ensure_list_key(&mut doc, &content_obj, "blocks")?;
            let Some((_, block_obj)) = find_block_obj(&doc, &blocks, &block_id)? else {
                anyhow::bail!("Block '{}' not found", block_id);
            };

            if index == 0 && delete == usize::MAX {
                let _ = doc.delete(&block_obj, "text");
                put_string(&mut doc, &block_obj, "text", &insert)?;
            } else {
                let text_obj = ensure_block_text_obj(&mut doc, &block_obj)?;
                let len = doc.length(&text_obj);
                let start = index.min(len);
                let max_delete = len.saturating_sub(start);
                let del = delete.min(max_delete);
                let del = isize::try_from(del).unwrap_or(isize::MAX);
                doc.splice_text(&text_obj, start, del, &insert)
                    .context("Failed to splice text")?;
            }
        }
    }

    Ok(doc.save())
}

/// Set content in v3 ThingMarkdown document
fn set_content_v3(doc: &mut AutoCommit, content_obj: &ObjId, content: Content) -> Result<()> {
    match content {
        Content::Text { blocks } => {
            put_string(doc, content_obj, "kind", "text")?;
            write_blocks_to_content(doc, content_obj, blocks)?;
        }
        Content::Markdown { blocks } => {
            put_string(doc, content_obj, "kind", "markdown")?;
            write_blocks_to_content(doc, content_obj, blocks)?;
        }
        Content::Opaque { kind, payload_json } => {
            put_string(doc, content_obj, "kind", &kind)?;
            put_string(doc, content_obj, "payload", &payload_json)?;
        }
    }
    Ok(())
}

fn write_blocks_to_content(
    doc: &mut AutoCommit,
    content_obj: &ObjId,
    blocks: Vec<Block>,
) -> Result<()> {
    let blocks_list = ensure_list_key(doc, content_obj, "blocks")?;

    // Clear existing blocks
    let len = doc.length(&blocks_list);
    for i in (0..len).rev() {
        let _ = doc.delete(&blocks_list, i);
    }

    // Insert new blocks
    for (i, b) in blocks.into_iter().enumerate() {
        let block_obj = doc
            .insert_object(&blocks_list, i, ObjType::Map)
            .context("Failed to insert block")?;
        put_string(doc, &block_obj, "id", &b.id)?;
        put_string(doc, &block_obj, "type", &b.r#type)?;
        if let Some(attrs) = b.attrs_json {
            put_string(doc, &block_obj, "attrs", &attrs)?;
        }
        if let Some(text) = b.text {
            set_block_text_chars(doc, &block_obj, &text)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests_v3 {
    use super::*;
    use crate::extract::{
        extract_collection_doc_view, extract_root_view, extract_thing_markdown_view,
    };

    #[test]
    fn test_root_op_add_collection() {
        let doc = Schema::init_root_doc("test").unwrap();
        let doc = apply_root_op(
            &doc,
            "test",
            RootOp::AddCollection {
                uuid: "coll-1".into(),
            },
        )
        .unwrap();
        let doc = apply_root_op(
            &doc,
            "test",
            RootOp::AddCollection {
                uuid: "coll-2".into(),
            },
        )
        .unwrap();

        let view = extract_root_view(&doc).unwrap();
        assert_eq!(view.collection_uuids.len(), 2);
        assert!(view.collection_uuids.contains(&"coll-1".to_string()));
        assert!(view.collection_uuids.contains(&"coll-2".to_string()));
    }

    #[test]
    fn test_root_op_remove_collection() {
        let doc = Schema::init_root_doc("test").unwrap();
        let doc = apply_root_op(
            &doc,
            "test",
            RootOp::AddCollection {
                uuid: "coll-1".into(),
            },
        )
        .unwrap();
        let doc = apply_root_op(
            &doc,
            "test",
            RootOp::AddCollection {
                uuid: "coll-2".into(),
            },
        )
        .unwrap();
        let doc = apply_root_op(
            &doc,
            "test",
            RootOp::RemoveCollection {
                uuid: "coll-1".into(),
            },
        )
        .unwrap();

        let view = extract_root_view(&doc).unwrap();
        assert_eq!(view.collection_uuids.len(), 1);
        assert!(view.collection_uuids.contains(&"coll-2".to_string()));
    }

    #[test]
    fn test_collection_op_upsert_thing() {
        let doc = Schema::init_collection_doc("test", "coll-1").unwrap();
        let doc = apply_collection_op(
            &doc,
            "test",
            "coll-1",
            CollectionOp::UpsertThingMeta {
                thing_id: "thing-1".into(),
                datatype: Some(ThingDatatype::Markdown),
                status: Some("in-progress".into()),
                status_timestamp_ms: None,
                title: Some("My Task".into()),
                parent_id: None,
                trigger: TriggerUpdate::Noop,
                built_in: None,
                attrs_json: None,
            },
        )
        .unwrap();

        let view = extract_collection_doc_view(&doc, "coll-1").unwrap();
        assert_eq!(view.things.len(), 1);
        assert_eq!(view.things[0].id, "thing-1");
        assert_eq!(view.things[0].title.as_deref(), Some("My Task"));
    }

    #[test]
    fn test_collection_op_rejects_missing_parent() {
        let doc = Schema::init_collection_doc("test", "coll-1").unwrap();
        let err = apply_collection_op(
            &doc,
            "test",
            "coll-1",
            CollectionOp::UpsertThingMeta {
                thing_id: "thing-1".into(),
                datatype: Some(ThingDatatype::Markdown),
                status: Some("none".into()),
                status_timestamp_ms: None,
                title: Some("Child".into()),
                parent_id: Some("missing-parent".into()),
                trigger: TriggerUpdate::Noop,
                built_in: None,
                attrs_json: None,
            },
        )
        .expect_err("missing parent must be rejected");

        assert!(
            err.to_string()
                .contains("must already exist in the same collection document"),
            "{err:?}"
        );
    }

    #[test]
    fn test_collection_op_rejects_parent_cycle() {
        let doc = Schema::init_collection_doc("test", "coll-1").unwrap();
        let doc = apply_collection_op(
            &doc,
            "test",
            "coll-1",
            CollectionOp::UpsertThingMeta {
                thing_id: "parent".into(),
                datatype: Some(ThingDatatype::Markdown),
                status: Some("none".into()),
                status_timestamp_ms: None,
                title: Some("Parent".into()),
                parent_id: None,
                trigger: TriggerUpdate::Noop,
                built_in: None,
                attrs_json: None,
            },
        )
        .unwrap();
        let doc = apply_collection_op(
            &doc,
            "test",
            "coll-1",
            CollectionOp::UpsertThingMeta {
                thing_id: "child".into(),
                datatype: Some(ThingDatatype::Markdown),
                status: Some("none".into()),
                status_timestamp_ms: None,
                title: Some("Child".into()),
                parent_id: Some("parent".into()),
                trigger: TriggerUpdate::Noop,
                built_in: None,
                attrs_json: None,
            },
        )
        .unwrap();

        let err = apply_collection_op(
            &doc,
            "test",
            "coll-1",
            CollectionOp::UpsertThingMeta {
                thing_id: "parent".into(),
                datatype: None,
                status: None,
                status_timestamp_ms: None,
                title: None,
                parent_id: Some("child".into()),
                trigger: TriggerUpdate::Noop,
                built_in: None,
                attrs_json: None,
            },
        )
        .expect_err("parent cycle must be rejected");

        assert!(err.to_string().contains("would create a cycle"), "{err:?}");
    }

    #[test]
    fn test_thing_markdown_op_set_content() {
        let doc = Schema::init_thing_markdown_doc("test", "thing-1").unwrap();
        let doc = apply_thing_markdown_op(
            &doc,
            "test",
            "thing-1",
            ThingMarkdownOp::SetContent {
                content: Content::Markdown {
                    blocks: vec![Block {
                        id: "b1".into(),
                        r#type: "paragraph".into(),
                        attrs_json: None,
                        text: Some("Hello world".into()),
                    }],
                },
            },
        )
        .unwrap();

        let view = extract_thing_markdown_view(&doc, "thing-1").unwrap();
        let content = view.content.unwrap();
        let blocks = content.blocks.unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text.as_deref(), Some("Hello world"));
    }
}
