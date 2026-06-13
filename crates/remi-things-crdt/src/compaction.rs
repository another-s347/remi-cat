use anyhow::{Context, Result};
use automerge::transaction::Transactable;
use automerge::{AutoCommit, ObjType, ReadDoc};

use crate::materialize::materialize_plan;
use crate::schema::{Schema, CURRENT_SCHEMA_VERSION};
use crate::util::{
    ensure_child_map, ensure_list_key, ensure_map_key, put_string, put_u64, set_doc_actor,
};
use crate::view::View;

/// Compact a document by rebuilding it from the extracted logical view.
///
/// This is the recommended GC mechanism for v2: server increments epoch and forces clients to reset.
pub fn compact_from_view(view: &View, actor: &str, new_epoch: u64) -> Result<Vec<u8>> {
    if view.schema_version != CURRENT_SCHEMA_VERSION {
        anyhow::bail!("Unsupported schema_version: {}", view.schema_version);
    }

    let mut doc = AutoCommit::new();
    set_doc_actor(&mut doc, actor);
    let (collections_map, things_map) = Schema::ensure_root(&mut doc, actor, new_epoch)?;

    // Collections
    for c in &view.collections {
        if c.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false) {
            continue;
        }
        let obj = ensure_child_map(&mut doc, &collections_map, &c.id)
            .context("Failed to create collection map")?;
        put_string(&mut doc, &obj, "title", &c.title)?;
        put_string(&mut doc, &obj, "status", &c.status)?;
        // clock
        let clock = ensure_map_key(&mut doc, &obj, "edit_clock")?;
        put_string(&mut doc, &clock, "actor", &c.edit_clock.actor)?;
        put_u64(&mut doc, &clock, "seq", c.edit_clock.seq)?;

        if let Some(trig) = &c.trigger {
            let t = ensure_map_key(&mut doc, &obj, "trigger")?;
            put_string(&mut doc, &t, "state", &trig.state)?;
            if let Some(uuid) = &trig.uuid {
                put_string(&mut doc, &t, "uuid", uuid)?;
            }
            let tc = ensure_map_key(&mut doc, &t, "clock")?;
            put_string(&mut doc, &tc, "actor", &trig.clock.actor)?;
            put_u64(&mut doc, &tc, "seq", trig.clock.seq)?;
        }

        if let Some(attrs) = &c.attrs {
            put_string(&mut doc, &obj, "attrs", &attrs.to_string())?;
        }
    }

    // Things
    for t in &view.things {
        if t.tombstone.as_ref().map(|x| x.deleted).unwrap_or(false) {
            continue;
        }
        let obj =
            ensure_child_map(&mut doc, &things_map, &t.id).context("Failed to create thing map")?;
        put_string(&mut doc, &obj, "collection_id", &t.collection_id)?;
        put_string(&mut doc, &obj, "datatype", t.datatype.as_str())?;
        put_string(&mut doc, &obj, "status", t.status.as_storage_str())?;
        if let Some(ts) = t.status.timestamp_ms() {
            put_u64(&mut doc, &obj, "status_timestamp_ms", ts as u64)?;
        }
        if let Some(title) = &t.title {
            put_string(&mut doc, &obj, "title", title)?;
        }
        if let Some(parent_id) = &t.parent_id {
            put_string(&mut doc, &obj, "parent_id", parent_id)?;
        }

        let clock = ensure_map_key(&mut doc, &obj, "edit_clock")?;
        put_string(&mut doc, &clock, "actor", &t.edit_clock.actor)?;
        put_u64(&mut doc, &clock, "seq", t.edit_clock.seq)?;

        if let Some(trig) = &t.trigger {
            let tr = ensure_map_key(&mut doc, &obj, "trigger")?;
            put_string(&mut doc, &tr, "state", &trig.state)?;
            if let Some(uuid) = &trig.uuid {
                put_string(&mut doc, &tr, "uuid", uuid)?;
            }
            let tc = ensure_map_key(&mut doc, &tr, "clock")?;
            put_string(&mut doc, &tc, "actor", &trig.clock.actor)?;
            put_u64(&mut doc, &tc, "seq", trig.clock.seq)?;
        }

        if let Some(attrs) = &t.attrs {
            put_string(&mut doc, &obj, "attrs", &attrs.to_string())?;
        }

        if let Some(content) = &t.content {
            let content_obj = ensure_map_key(&mut doc, &obj, "content")?;
            put_string(&mut doc, &content_obj, "kind", &content.kind)?;
            if let Some(payload) = &content.payload {
                put_string(&mut doc, &content_obj, "payload", &payload.to_string())?;
            }
            if let Some(blocks) = &content.blocks {
                let list = ensure_list_key(&mut doc, &content_obj, "blocks")?;
                for (i, b) in blocks.iter().enumerate() {
                    let block_obj = doc
                        .insert_object(&list, i, ObjType::Map)
                        .context("Failed to insert block")?;
                    put_string(&mut doc, &block_obj, "id", &b.id)?;
                    put_string(&mut doc, &block_obj, "type", &b.r#type)?;
                    if let Some(attrs) = &b.attrs {
                        put_string(&mut doc, &block_obj, "attrs", &attrs.to_string())?;
                    }
                    if let Some(text) = &b.text {
                        let text_obj = doc
                            .put_object(&block_obj, "text", ObjType::Text)
                            .context("Failed to create block.text Text")?;
                        doc.splice_text(&text_obj, 0, 0, text)
                            .context("Failed to write block text")?;
                    }
                }
            }
        }
    }

    // MaterializePlan is not applied here, but we sanity-call it to keep the API stable.
    let _plan = materialize_plan(view);

    Ok(doc.save())
}

// ============================================================================
// V3 Multi-Document Compaction
// ============================================================================

use crate::extract::{
    extract_collection_doc_view_from_doc, extract_root_view_from_doc,
    extract_thing_content_view_from_doc, extract_thing_markdown_view_from_doc,
};
use crate::view::{CollectionDocView, RootView, ThingContentView, ThingMarkdownView};

/// Default compaction threshold in bytes (64KB)
pub const DEFAULT_COMPACTION_THRESHOLD: usize = 64 * 1024;

/// Check if a document needs compaction based on size
pub fn needs_compaction(doc_bytes: &[u8], threshold: usize) -> bool {
    doc_bytes.len() > threshold
}

/// Compact a Root document by rebuilding from its view
pub fn compact_root_doc(doc_bytes: &[u8], actor: &str) -> Result<Vec<u8>> {
    let doc = automerge::AutoCommit::load(doc_bytes)
        .context("Failed to load root document for compaction")?;

    let view =
        extract_root_view_from_doc(&doc).context("Failed to extract root view for compaction")?;

    rebuild_root_doc(&view, actor)
}

/// Rebuild a Root document from its view
fn rebuild_root_doc(view: &RootView, actor: &str) -> Result<Vec<u8>> {
    let mut doc = AutoCommit::new();
    set_doc_actor(&mut doc, actor);

    // Create root structure
    let root = doc
        .put_object(automerge::ROOT, "root", ObjType::Map)
        .context("Failed to create root map")?;

    put_u64(
        &mut doc,
        &root,
        "schema_version",
        view.schema_version as u64,
    )?;
    put_u64(&mut doc, &root, "epoch", view.epoch)?;

    // Create collection_uuids list
    let list = doc
        .put_object(&root, "collection_uuids", ObjType::List)
        .context("Failed to create collection_uuids list")?;

    for (i, uuid) in view.collection_uuids.iter().enumerate() {
        doc.insert(&list, i, uuid.as_str())
            .context("Failed to insert collection uuid")?;
    }

    Ok(doc.save())
}

/// Compact a Collection document by rebuilding from its view
///
/// # Arguments
/// * `doc_bytes` - The document bytes to compact
/// * `collection_uuid` - The collection UUID (used for extraction)
/// * `actor` - The actor ID for the new document
pub fn compact_collection_doc(
    doc_bytes: &[u8],
    collection_uuid: &str,
    actor: &str,
) -> Result<Vec<u8>> {
    let doc = automerge::AutoCommit::load(doc_bytes)
        .context("Failed to load collection document for compaction")?;

    let view = extract_collection_doc_view_from_doc(&doc, collection_uuid)
        .context("Failed to extract collection doc view for compaction")?;

    rebuild_collection_doc(&view, actor)
}

/// Rebuild a Collection document from its view
fn rebuild_collection_doc(view: &CollectionDocView, actor: &str) -> Result<Vec<u8>> {
    // Start from the init template and update values
    let template = Schema::init_collection_doc(actor, &view.meta.id)?;
    let mut doc =
        automerge::AutoCommit::load(&template).context("Failed to load collection template")?;

    // Update collection metadata in meta object
    if let Some((automerge::Value::Object(ObjType::Map), meta_obj)) =
        doc.get(automerge::ROOT, Schema::KEY_META)?
    {
        put_string(&mut doc, &meta_obj, "title", &view.meta.title)?;
        put_string(&mut doc, &meta_obj, "status", &view.meta.status)?;

        // Edit clock
        let clock = ensure_map_key(&mut doc, &meta_obj, "edit_clock")?;
        put_string(&mut doc, &clock, "actor", &view.meta.edit_clock.actor)?;
        put_u64(&mut doc, &clock, "seq", view.meta.edit_clock.seq)?;

        // Trigger
        if let Some(trig) = &view.meta.trigger {
            let t = ensure_map_key(&mut doc, &meta_obj, "trigger")?;
            put_string(&mut doc, &t, "state", &trig.state)?;
            if let Some(uuid) = &trig.uuid {
                put_string(&mut doc, &t, "uuid", uuid)?;
            }
            let tc = ensure_map_key(&mut doc, &t, "clock")?;
            put_string(&mut doc, &tc, "actor", &trig.clock.actor)?;
            put_u64(&mut doc, &tc, "seq", trig.clock.seq)?;
        }
    }

    // Things metadata
    if let Some((automerge::Value::Object(ObjType::Map), things_map)) =
        doc.get(automerge::ROOT, Schema::KEY_THING_MAP)?
    {
        for thing in &view.things {
            // Skip tombstoned things
            if thing.tombstone.as_ref().map(|t| t.deleted).unwrap_or(false) {
                continue;
            }

            let obj = ensure_child_map(&mut doc, &things_map, &thing.id)?;
            put_string(&mut doc, &obj, "datatype", thing.datatype.as_str())?;
            put_string(&mut doc, &obj, "status", thing.status.as_storage_str())?;
            if let Some(ts) = thing.status.timestamp_ms() {
                put_u64(&mut doc, &obj, "status_timestamp_ms", ts as u64)?;
            }
            if let Some(title) = &thing.title {
                put_string(&mut doc, &obj, "title", title)?;
            }
            if let Some(parent_id) = &thing.parent_id {
                put_string(&mut doc, &obj, "parent_id", parent_id)?;
            }

            // Edit clock
            let clock = ensure_map_key(&mut doc, &obj, "edit_clock")?;
            put_string(&mut doc, &clock, "actor", &thing.edit_clock.actor)?;
            put_u64(&mut doc, &clock, "seq", thing.edit_clock.seq)?;

            // Trigger
            if let Some(trig) = &thing.trigger {
                let tr = ensure_map_key(&mut doc, &obj, "trigger")?;
                put_string(&mut doc, &tr, "state", &trig.state)?;
                if let Some(uuid) = &trig.uuid {
                    put_string(&mut doc, &tr, "uuid", uuid)?;
                }
                let tc = ensure_map_key(&mut doc, &tr, "clock")?;
                put_string(&mut doc, &tc, "actor", &trig.clock.actor)?;
                put_u64(&mut doc, &tc, "seq", trig.clock.seq)?;
            }
        }
    }

    Ok(doc.save())
}

/// Compact a ThingMarkdown document by rebuilding from its view
///
/// # Arguments
/// * `doc_bytes` - The document bytes to compact
/// * `thing_uuid` - The thing UUID (used for extraction)
/// * `actor` - The actor ID for the new document
pub fn compact_thing_markdown_doc(
    doc_bytes: &[u8],
    thing_uuid: &str,
    actor: &str,
) -> Result<Vec<u8>> {
    let doc = automerge::AutoCommit::load(doc_bytes)
        .context("Failed to load thing markdown document for compaction")?;

    let view = extract_thing_markdown_view_from_doc(&doc, thing_uuid)
        .context("Failed to extract thing markdown view for compaction")?;

    rebuild_thing_markdown_doc(&view, actor)
}

/// Compact a generic thing-owned content document by rebuilding from its view.
pub fn compact_thing_content_doc(
    doc_bytes: &[u8],
    document_uuid: &str,
    thing_uuid: &str,
    actor: &str,
) -> Result<Vec<u8>> {
    let doc = automerge::AutoCommit::load(doc_bytes)
        .context("Failed to load thing content document for compaction")?;

    let view = extract_thing_content_view_from_doc(&doc, document_uuid, thing_uuid)
        .context("Failed to extract thing content view for compaction")?;

    rebuild_thing_content_doc(&view, actor)
}

/// Rebuild a ThingMarkdown document from its view
fn rebuild_thing_markdown_doc(view: &ThingMarkdownView, actor: &str) -> Result<Vec<u8>> {
    let content_view = ThingContentView {
        schema_version: view.schema_version,
        document_uuid: view.thing_uuid.clone(),
        thing_uuid: view.thing_uuid.clone(),
        content_type: "markdown".to_string(),
        content: view.content.clone(),
    };
    rebuild_thing_content_doc(&content_view, actor)
}

fn rebuild_thing_content_doc(view: &ThingContentView, actor: &str) -> Result<Vec<u8>> {
    // Start from the init template and update values
    let template = Schema::init_thing_content_doc(
        actor,
        &view.document_uuid,
        &view.thing_uuid,
        &view.content_type,
    )?;
    let mut doc =
        automerge::AutoCommit::load(&template).context("Failed to load thing content template")?;

    // If there's content, rebuild it
    if let Some(content) = &view.content {
        if let Some((automerge::Value::Object(ObjType::Map), content_obj)) =
            doc.get(automerge::ROOT, Schema::KEY_CONTENT)?
        {
            put_string(&mut doc, &content_obj, "kind", &content.kind)?;

            if let Some(payload) = &content.payload {
                put_string(&mut doc, &content_obj, "payload", &payload.to_string())?;
            }

            if let Some(blocks) = &content.blocks {
                let list = ensure_list_key(&mut doc, &content_obj, "blocks")?;
                for (i, b) in blocks.iter().enumerate() {
                    let block_obj = doc
                        .insert_object(&list, i, ObjType::Map)
                        .context("Failed to insert block")?;
                    put_string(&mut doc, &block_obj, "id", &b.id)?;
                    put_string(&mut doc, &block_obj, "type", &b.r#type)?;
                    if let Some(attrs) = &b.attrs {
                        put_string(&mut doc, &block_obj, "attrs", &attrs.to_string())?;
                    }
                    if let Some(text) = &b.text {
                        let text_obj = doc
                            .put_object(&block_obj, "text", ObjType::Text)
                            .context("Failed to create block.text Text")?;
                        doc.splice_text(&text_obj, 0, 0, text)
                            .context("Failed to write block text")?;
                    }
                }
            }
        }
    }

    Ok(doc.save())
}
