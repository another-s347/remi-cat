use anyhow::{Context, Result};
use automerge::{AutoCommit, ObjId, ObjType, ReadDoc, ScalarValue, Value};
use std::time::Instant;
use tracing::info;

use crate::datatype::{
    ContentEntry, ContentEntryPayload, DateField, ImageField, LocationField, UrlField,
};
use crate::schema::{Schema, CURRENT_SCHEMA_VERSION};
use crate::util::{
    collect_list_keys, collect_map_keys, collect_root_maps, get_json_string, get_string, get_u64,
};
use crate::view::{
    BlockView, CollectionDocView, CollectionMetaView, CollectionView, ContentView, EditClock,
    RootView, ThingBuiltInFieldsView, ThingContentView, ThingMarkdownView, ThingMetaView,
    ThingStatus, ThingView, Tombstone, TriggerBinding, View,
};
use crate::ThingDatatype;

/// Extract the markdown text for a single Thing (best-effort).
///
/// This is an on-demand read path used by tools/agent requests to avoid extracting
/// (and serializing) every Thing's content.
///
/// Returns the first block with text (prefers `main` when present).
pub fn extract_thing_markdown(doc_bytes: &[u8], thing_id: &str) -> Result<Option<String>> {
    if doc_bytes.is_empty() {
        return Ok(None);
    }

    let doc = AutoCommit::load(doc_bytes).context("Failed to load automerge doc")?;
    extract_thing_markdown_from_doc(&doc, thing_id)
}

/// Extract the markdown text for a single Thing from an already-loaded document.
pub fn extract_thing_markdown_from_doc(doc: &AutoCommit, thing_id: &str) -> Result<Option<String>> {
    let thing_id = thing_id.trim();
    if thing_id.is_empty() {
        return Ok(None);
    }

    // Find the thing object by id; choose the best candidate by edit clock.
    let root_maps = collect_root_maps(doc, Schema::KEY_THINGS)?;
    let mut candidates: Vec<ObjId> = Vec::new();
    for map in root_maps {
        if let Ok(all) = doc.get_all(&map, thing_id) {
            for (val, obj) in all {
                if matches!(val, Value::Object(ObjType::Map)) {
                    candidates.push(obj);
                }
            }
        }
    }

    let Some(best) = pick_best_by_clock(doc, &candidates)? else {
        return Ok(None);
    };

    let Some(content) = read_content(doc, &best)? else {
        return Ok(None);
    };
    let Some(blocks) = content.blocks else {
        return Ok(None);
    };

    // Prefer `main` if present.
    if let Some(main) = blocks.iter().find(|b| b.id == "main") {
        if let Some(text) = main.text.as_ref() {
            if !text.is_empty() {
                return Ok(Some(text.clone()));
            }
        }
    }

    // Otherwise, return the first block with text.
    for b in blocks {
        if let Some(text) = b.text {
            if !text.is_empty() {
                return Ok(Some(text));
            }
        }
    }

    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractOptions {
    /// Whether to extract things at all. If false, `View.things` will be empty.
    pub include_things: bool,
    /// Whether to extract thing content (blocks/text/payload). If false, `ThingView.content` will be None.
    pub include_content: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            include_things: true,
            include_content: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocScale {
    pub doc_bytes: usize,
    pub changes: usize,
    pub ops: usize,
    pub heads: usize,
}

pub fn extract_view(doc_bytes: &[u8]) -> Result<View> {
    Ok(extract_view_with_scale(doc_bytes)?.0)
}

pub fn extract_view_with_scale(doc_bytes: &[u8]) -> Result<(View, DocScale)> {
    extract_view_with_options_and_scale(doc_bytes, ExtractOptions::default())
}

pub fn extract_view_with_options_and_scale(
    doc_bytes: &[u8],
    options: ExtractOptions,
) -> Result<(View, DocScale)> {
    let total = Instant::now();

    let t0 = Instant::now();
    let mut doc = if doc_bytes.is_empty() {
        AutoCommit::new()
    } else {
        AutoCommit::load(doc_bytes).context("Failed to load automerge doc")?
    };

    // Rough scale metrics for diagnosing performance:
    // - `doc_bytes`: persisted doc byte size
    // - `changes`: number of Automerge changes in the doc
    // - `ops`: total ops across all changes (proxy for "op count")
    // - `heads`: number of heads in the doc
    let (changes_len, ops) = {
        let changes = doc.get_changes(&[]);
        let ops: usize = changes.iter().map(|c| c.len()).sum();
        (changes.len(), ops)
    };
    let heads_len = doc.get_heads().len();
    info!(
        doc_bytes = doc_bytes.len(),
        changes = changes_len,
        ops,
        heads = heads_len,
        ms = t0.elapsed().as_millis(),
        "extract_view: load"
    );

    let t1 = Instant::now();
    let out = extract_view_from_doc_with_options(&doc, options)?;
    info!(
        collections = out.collections.len(),
        things = out.things.len(),
        ms = t1.elapsed().as_millis(),
        total_ms = total.elapsed().as_millis(),
        "extract_view: extract_view_from_doc"
    );

    Ok((
        out,
        DocScale {
            doc_bytes: doc_bytes.len(),
            changes: changes_len,
            ops,
            heads: heads_len,
        },
    ))
}

/// Extract view from an already-loaded AutoCommit document.
pub fn extract_view_from_doc(doc: &AutoCommit) -> Result<View> {
    extract_view_from_doc_with_options(doc, ExtractOptions::default())
}

/// Extract view from an already-loaded AutoCommit document, with options.
pub fn extract_view_from_doc_with_options(
    doc: &AutoCommit,
    options: ExtractOptions,
) -> Result<View> {
    let total = Instant::now();
    let schema_version = get_u64(doc, &automerge::ROOT, Schema::KEY_SCHEMA_VERSION)?
        .unwrap_or(CURRENT_SCHEMA_VERSION as u64) as u32;
    let epoch = get_u64(doc, &automerge::ROOT, Schema::KEY_EPOCH)?.unwrap_or(0);

    let t0 = Instant::now();
    let collections = extract_collections(doc)?;
    info!(
        collections = collections.len(),
        ms = t0.elapsed().as_millis(),
        "extract_view_from_doc: extract_collections"
    );

    let t1 = Instant::now();
    let things = if options.include_things {
        extract_things(doc, options.include_content)?
    } else {
        Vec::new()
    };
    info!(
        things = things.len(),
        ms = t1.elapsed().as_millis(),
        "extract_view_from_doc: extract_things"
    );

    info!(
        schema_version,
        epoch,
        total_ms = total.elapsed().as_millis(),
        "extract_view_from_doc: done"
    );

    Ok(View {
        schema_version,
        epoch,
        collections,
        things,
    })
}

fn extract_collections(doc: &AutoCommit) -> Result<Vec<CollectionView>> {
    let root_maps = collect_root_maps(doc, Schema::KEY_COLLECTIONS)?;
    let mut by_id: std::collections::BTreeMap<String, ObjId> = Default::default();

    for map in root_maps {
        for id in doc.keys(&map) {
            let mut candidates: Vec<ObjId> = Vec::new();
            if let Ok(all) = doc.get_all(&map, id.as_str()) {
                for (val, obj) in all {
                    if matches!(val, Value::Object(ObjType::Map)) {
                        candidates.push(obj);
                    }
                }
            }

            let Some(best) = pick_best_by_clock(doc, &candidates)? else {
                continue;
            };

            match by_id.get(id.as_str()) {
                None => {
                    by_id.insert(id.to_string(), best);
                }
                Some(existing) => {
                    let eb = read_edit_clock(doc, existing)?;
                    let nb = read_edit_clock(doc, &best)?;
                    if nb > eb {
                        by_id.insert(id.to_string(), best);
                    }
                }
            }
        }
    }

    let mut out = Vec::with_capacity(by_id.len());
    for (id, obj) in by_id {
        let title = get_string(doc, &obj, "title")?.unwrap_or_default();
        let status = get_string(doc, &obj, "status")?.unwrap_or_else(|| "active".to_string());
        let edit_clock = read_edit_clock(doc, &obj)?;
        let tombstone = read_tombstone(doc, &obj)?;
        let trigger = read_trigger(doc, &obj)?;
        let attrs = get_json_string(doc, &obj, "attrs")?;

        out.push(CollectionView {
            id,
            title,
            status,
            edit_clock,
            tombstone,
            trigger,
            attrs,
        });
    }

    Ok(out)
}

fn extract_things(doc: &AutoCommit, include_content: bool) -> Result<Vec<ThingView>> {
    let root_maps = collect_root_maps(doc, Schema::KEY_THINGS)?;
    let mut by_id: std::collections::BTreeMap<String, ObjId> = Default::default();

    for map in root_maps {
        for id in doc.keys(&map) {
            let mut candidates: Vec<ObjId> = Vec::new();
            if let Ok(all) = doc.get_all(&map, id.as_str()) {
                for (val, obj) in all {
                    if matches!(val, Value::Object(ObjType::Map)) {
                        candidates.push(obj);
                    }
                }
            }

            let Some(best) = pick_best_by_clock(doc, &candidates)? else {
                continue;
            };

            match by_id.get(id.as_str()) {
                None => {
                    by_id.insert(id.to_string(), best);
                }
                Some(existing) => {
                    let eb = read_edit_clock(doc, existing)?;
                    let nb = read_edit_clock(doc, &best)?;
                    if nb > eb {
                        by_id.insert(id.to_string(), best);
                    }
                }
            }
        }
    }

    let mut out = Vec::with_capacity(by_id.len());
    for (id, obj) in by_id {
        let collection_id = get_string(doc, &obj, "collection_id")?.unwrap_or_default();
        let datatype = ThingDatatype::from_str(
            &get_string(doc, &obj, "datatype")?.unwrap_or_else(|| "markdown".to_string()),
        );
        let status = read_thing_status(doc, &obj)?;
        let title = get_string(doc, &obj, "title")?;
        let parent_id = get_string(doc, &obj, "parent_id")?;
        let edit_clock = read_edit_clock(doc, &obj)?;
        let tombstone = read_tombstone(doc, &obj)?;
        let trigger = read_trigger(doc, &obj)?;
        let attrs = get_json_string(doc, &obj, "attrs")?;
        let content = if include_content {
            read_content(doc, &obj)?
        } else {
            None
        };

        out.push(ThingView {
            id,
            collection_id,
            datatype,
            status,
            edit_clock,
            tombstone,
            title,
            parent_id,
            trigger,
            content,
            attrs,
        });
    }

    Ok(out)
}

fn pick_best_by_clock(doc: &AutoCommit, candidates: &[ObjId]) -> Result<Option<ObjId>> {
    let mut best: Option<(EditClock, ObjId)> = None;
    for obj in candidates {
        let c = read_edit_clock(doc, obj)?;
        match &best {
            None => best = Some((c, obj.clone())),
            Some((best_c, _)) if c > *best_c => best = Some((c, obj.clone())),
            _ => {}
        }
    }
    Ok(best.map(|(_, o)| o))
}

fn read_edit_clock(doc: &AutoCommit, entity_obj: &ObjId) -> Result<EditClock> {
    let Some((Value::Object(ObjType::Map), clock_obj)) = doc.get(entity_obj, "edit_clock")? else {
        return Ok(EditClock::new("", 0));
    };

    let actor = get_string(doc, &clock_obj, "actor")?.unwrap_or_default();
    let seq = get_u64(doc, &clock_obj, "seq")?.unwrap_or(0);
    Ok(EditClock::new(actor, seq))
}

fn read_tombstone(doc: &AutoCommit, entity_obj: &ObjId) -> Result<Option<Tombstone>> {
    let Some((Value::Object(ObjType::Map), tomb_obj)) = doc.get(entity_obj, "tombstone")? else {
        return Ok(None);
    };

    let deleted = match doc.get(&tomb_obj, "deleted")? {
        Some((Value::Scalar(sv), _)) => match sv.as_ref() {
            ScalarValue::Boolean(b) => *b,
            ScalarValue::Int(i) => *i != 0,
            ScalarValue::Uint(u) => *u != 0,
            _ => false,
        },
        None => false,
        _ => false,
    };

    let clock =
        if let Some((Value::Object(ObjType::Map), clock_obj)) = doc.get(&tomb_obj, "clock")? {
            EditClock::new(
                get_string(doc, &clock_obj, "actor")?.unwrap_or_default(),
                get_u64(doc, &clock_obj, "seq")?.unwrap_or(0),
            )
        } else {
            EditClock::new("", 0)
        };

    Ok(Some(Tombstone { deleted, clock }))
}

fn read_trigger(doc: &AutoCommit, entity_obj: &ObjId) -> Result<Option<TriggerBinding>> {
    let Some((Value::Object(ObjType::Map), trig_obj)) = doc.get(entity_obj, "trigger")? else {
        return Ok(None);
    };

    let state = get_string(doc, &trig_obj, "state")?.unwrap_or_else(|| "none".to_string());
    let uuid = get_string(doc, &trig_obj, "uuid")?
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let clock =
        if let Some((Value::Object(ObjType::Map), clock_obj)) = doc.get(&trig_obj, "clock")? {
            EditClock::new(
                get_string(doc, &clock_obj, "actor")?.unwrap_or_default(),
                get_u64(doc, &clock_obj, "seq")?.unwrap_or(0),
            )
        } else {
            EditClock::new("", 0)
        };

    Ok(Some(TriggerBinding { state, uuid, clock }))
}

fn read_thing_status(doc: &AutoCommit, entity_obj: &ObjId) -> Result<ThingStatus> {
    let status_str = get_string(doc, entity_obj, "status")?.unwrap_or_else(|| "none".to_string());

    // Try to read timestamp from status_timestamp_ms field
    let timestamp_ms = get_u64(doc, entity_obj, "status_timestamp_ms")?.map(|v| v as i64);

    Ok(ThingStatus::from_storage(&status_str, timestamp_ms))
}

fn read_content(doc: &AutoCommit, entity_obj: &ObjId) -> Result<Option<ContentView>> {
    let Some((Value::Object(ObjType::Map), content_obj)) = doc.get(entity_obj, "content")? else {
        return Ok(None);
    };

    let kind = get_string(doc, &content_obj, "kind")?.unwrap_or_else(|| "text".to_string());
    let payload = get_json_string(doc, &content_obj, "payload")?;

    let blocks = if let Some((Value::Object(ObjType::List), blocks_list)) =
        doc.get(&content_obj, "blocks")?
    {
        let mut out = Vec::new();
        for i in 0..doc.length(&blocks_list) {
            let Some((Value::Object(ObjType::Map), block_obj)) = doc.get(&blocks_list, i)? else {
                continue;
            };
            let id = get_string(doc, &block_obj, "id")?.unwrap_or_default();
            let typ =
                get_string(doc, &block_obj, "type")?.unwrap_or_else(|| "paragraph".to_string());
            let attrs = get_json_string(doc, &block_obj, "attrs")?;
            let text = read_block_text_chars(doc, &block_obj)?;
            out.push(BlockView {
                id,
                r#type: typ,
                attrs,
                text,
            });
        }
        Some(out)
    } else {
        None
    };

    Ok(Some(ContentView {
        kind,
        blocks,
        payload,
    }))
}

fn read_block_text_chars(doc: &AutoCommit, block_obj: &ObjId) -> Result<Option<String>> {
    // v2 legacy: text stored as List of per-char strings
    // v2+ (optimized): text stored as a Scalar string (deprecated)
    // v2+ (CRDT-correct): text stored as an Automerge Text object
    let Some((val, obj)) = doc.get(block_obj, "text")? else {
        return Ok(None);
    };

    match val {
        Value::Object(ObjType::Text) => {
            let text_obj = obj;
            let s = doc.text(&text_obj).context("Failed to read Text")?;
            Ok(Some(s))
        }
        Value::Object(ObjType::List) => {
            let list = obj;
            let mut out = String::new();
            for i in 0..doc.length(&list) {
                match doc.get(&list, i)? {
                    Some((Value::Scalar(sv), _)) => match sv.as_ref() {
                        ScalarValue::Str(s) => out.push_str(s),
                        ScalarValue::Bytes(b) => out.push_str(&String::from_utf8_lossy(b)),
                        _ => {}
                    },
                    _ => {}
                }
            }
            Ok(Some(out))
        }
        Value::Scalar(sv) => match sv.as_ref() {
            ScalarValue::Str(s) => Ok(Some(s.to_string())),
            ScalarValue::Bytes(b) => Ok(Some(String::from_utf8_lossy(b).to_string())),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

// ============================================================================
// V3 Multi-Document Extraction Functions
// ============================================================================

/// Extract view from a Root document (CrdtDataType::Root)
pub fn extract_root_view(doc_bytes: &[u8]) -> Result<RootView> {
    if doc_bytes.is_empty() {
        return Ok(RootView {
            schema_version: CURRENT_SCHEMA_VERSION,
            epoch: 0,
            collection_uuids: Vec::new(),
        });
    }

    let doc = AutoCommit::load(doc_bytes).context("Failed to load root document")?;
    extract_root_view_from_doc(&doc)
}

/// Extract view from a Root document (already loaded)
///
/// When two devices independently create a root document, Automerge creates
/// conflicting values for the `collection_uuids` list key.  `doc.get()` would
/// return only one of the competing lists (chosen by actor-ID ordering), which
/// can be the empty one—losing all data.  We use `doc.get_all()` to read
/// **every** conflicting list and merge them with deduplication so that no
/// collection is silently dropped.
pub fn extract_root_view_from_doc(doc: &AutoCommit) -> Result<RootView> {
    let schema_version = get_u64(doc, &automerge::ROOT, Schema::KEY_SCHEMA_VERSION)?
        .unwrap_or(CURRENT_SCHEMA_VERSION as u64) as u32;
    let epoch = get_u64(doc, &automerge::ROOT, Schema::KEY_EPOCH)?.unwrap_or(0);

    // Read collection_uuids list, merging ALL conflicting values so that
    // independently-created root documents don't lose data.
    let collection_uuids = {
        let mut uuids = Vec::new();
        let mut seen = std::collections::HashSet::new();

        if let Ok(all_values) = doc.get_all(automerge::ROOT, Schema::KEY_COLLECTION_UUIDS) {
            for (value, obj_id) in all_values {
                if matches!(&value, Value::Object(ObjType::List)) {
                    for i in 0..doc.length(&obj_id) {
                        if let Some((Value::Scalar(sv), _)) = doc.get(&obj_id, i)? {
                            if let ScalarValue::Str(s) = sv.as_ref() {
                                let uuid = s.to_string();
                                if seen.insert(uuid.clone()) {
                                    uuids.push(uuid);
                                }
                            }
                        }
                    }
                }
            }
        }

        uuids
    };

    Ok(RootView {
        schema_version,
        epoch,
        collection_uuids,
    })
}

/// Extract view from a Collection document (CrdtDataType::Collection)
pub fn extract_collection_doc_view(
    doc_bytes: &[u8],
    collection_uuid: &str,
) -> Result<CollectionDocView> {
    if doc_bytes.is_empty() {
        return Ok(CollectionDocView {
            schema_version: CURRENT_SCHEMA_VERSION,
            meta: CollectionMetaView {
                id: collection_uuid.to_string(),
                title: String::new(),
                status: "active".to_string(),
                edit_clock: EditClock::zero(),
                tombstone: None,
                trigger: None,
                attrs: None,
            },
            things: Vec::new(),
        });
    }

    let doc = AutoCommit::load(doc_bytes).context("Failed to load collection document")?;
    extract_collection_doc_view_from_doc(&doc, collection_uuid)
}

/// Extract view from a Collection document (already loaded)
pub fn extract_collection_doc_view_from_doc(
    doc: &AutoCommit,
    collection_uuid: &str,
) -> Result<CollectionDocView> {
    let schema_version = get_u64(doc, &automerge::ROOT, Schema::KEY_SCHEMA_VERSION)?
        .unwrap_or(CURRENT_SCHEMA_VERSION as u64) as u32;

    // Read meta
    let meta = if let Some((Value::Object(ObjType::Map), meta_obj)) =
        doc.get(automerge::ROOT, Schema::KEY_META)?
    {
        let id = get_string(doc, &meta_obj, "id")?.unwrap_or_else(|| collection_uuid.to_string());
        let title = get_string(doc, &meta_obj, "title")?.unwrap_or_default();
        let status = get_string(doc, &meta_obj, "status")?.unwrap_or_else(|| "active".to_string());
        let edit_clock = read_edit_clock(doc, &meta_obj)?;
        let tombstone = read_tombstone(doc, &meta_obj)?;
        let trigger = read_trigger(doc, &meta_obj)?;
        let attrs = get_json_string(doc, &meta_obj, "attrs")?;

        CollectionMetaView {
            id,
            title,
            status,
            edit_clock,
            tombstone,
            trigger,
            attrs,
        }
    } else {
        CollectionMetaView {
            id: collection_uuid.to_string(),
            title: String::new(),
            status: "active".to_string(),
            edit_clock: EditClock::zero(),
            tombstone: None,
            trigger: None,
            attrs: None,
        }
    };

    // Read things map
    let things = if let Some((Value::Object(ObjType::Map), things_map)) =
        doc.get(automerge::ROOT, Schema::KEY_THING_MAP)?
    {
        let mut out = Vec::new();
        for thing_id in doc.keys(&things_map) {
            if let Some((Value::Object(ObjType::Map), thing_obj)) =
                doc.get(&things_map, thing_id.as_str())?
            {
                let thing_meta = extract_thing_meta_from_obj(doc, &thing_obj, &thing_id)?;
                out.push(thing_meta);
            }
        }
        out
    } else {
        Vec::new()
    };

    Ok(CollectionDocView {
        schema_version,
        meta,
        things,
    })
}

/// Extract ThingMetaView from a thing object in a Collection document
fn extract_thing_meta_from_obj(
    doc: &AutoCommit,
    thing_obj: &ObjId,
    thing_id: &str,
) -> Result<ThingMetaView> {
    let datatype = ThingDatatype::from_str(
        &get_string(doc, thing_obj, "datatype")?.unwrap_or_else(|| "markdown".to_string()),
    );
    let status = read_thing_status(doc, thing_obj)?;
    let title = get_string(doc, thing_obj, "title")?;
    let parent_id = get_string(doc, thing_obj, "parent_id")?;
    let edit_clock = read_edit_clock(doc, thing_obj)?;
    let tombstone = read_tombstone(doc, thing_obj)?;
    let trigger = read_trigger(doc, thing_obj)?;
    let attrs = get_json_string(doc, thing_obj, "attrs")?;

    // Read built_in fields
    let built_in_objs = collect_map_keys(doc, thing_obj, Schema::KEY_BUILT_IN)?;
    let built_in = if built_in_objs.is_empty() {
        ThingBuiltInFieldsView::default()
    } else {
        extract_built_in_fields(doc, &built_in_objs)?
    };

    Ok(ThingMetaView {
        id: thing_id.to_string(),
        datatype,
        status,
        edit_clock,
        tombstone,
        title,
        parent_id,
        trigger,
        built_in,
        attrs,
    })
}

/// Extract ThingBuiltInFieldsView from a built_in object (V3 multi-value content entries)
fn extract_built_in_fields(
    doc: &AutoCommit,
    built_in_objs: &[ObjId],
) -> Result<ThingBuiltInFieldsView> {
    let extra = built_in_objs
        .iter()
        .find_map(|built_in_obj| get_json_string(doc, built_in_obj, "extra").ok().flatten());

    let mut content_entries_by_id = std::collections::BTreeMap::new();
    for built_in_obj in built_in_objs {
        for entries_list in collect_list_keys(doc, built_in_obj, "content_entries")? {
            let len = doc.length(&entries_list);
            for i in 0..len {
                if let Some((Value::Object(ObjType::Map), entry_obj)) = doc.get(&entries_list, i)? {
                    if let Some(entry) = extract_content_entry(doc, &entry_obj)? {
                        content_entries_by_id.insert(entry.id.clone(), entry);
                    }
                }
            }
        }
    }

    let mut content_entries: Vec<_> = content_entries_by_id.into_values().collect();

    // Sort by order
    content_entries.sort_by(|a, b| {
        a.order
            .partial_cmp(&b.order)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(ThingBuiltInFieldsView {
        content_entries,
        extra,
    })
}

/// Extract a single ContentEntry from an entry object
fn extract_content_entry(doc: &AutoCommit, entry_obj: &ObjId) -> Result<Option<ContentEntry>> {
    let id = match get_string(doc, entry_obj, "id")? {
        Some(id) => id,
        None => return Ok(None), // Invalid entry without ID
    };

    let title = get_string(doc, entry_obj, "title")?;
    let order = get_f64(doc, entry_obj, "order")?.unwrap_or(0.0);

    // Read payload
    let payload =
        if let Some((Value::Object(ObjType::Map), payload_obj)) = doc.get(entry_obj, "payload")? {
            extract_content_entry_payload(doc, &payload_obj)?
        } else {
            return Ok(None); // Invalid entry without payload
        };

    let payload = match payload {
        Some(p) => p,
        None => return Ok(None),
    };

    Ok(Some(ContentEntry {
        id,
        title,
        order,
        payload,
    }))
}

/// Extract ContentEntryPayload from a payload object
fn extract_content_entry_payload(
    doc: &AutoCommit,
    payload_obj: &ObjId,
) -> Result<Option<ContentEntryPayload>> {
    let payload_type = get_string(doc, payload_obj, "type")?.unwrap_or_default();

    match payload_type.as_str() {
        "location" => {
            let loc_type = get_string(doc, payload_obj, "loc_type")?.unwrap_or_default();
            let location = match loc_type.as_str() {
                "coordinate" => {
                    let lat = get_f64(doc, payload_obj, "lat")?.unwrap_or(0.0);
                    let lng = get_f64(doc, payload_obj, "lng")?.unwrap_or(0.0);
                    let coord_system = get_string(doc, payload_obj, "coord_system")?
                        .unwrap_or_else(|| "wgs84".to_string());
                    let source_name = get_string(doc, payload_obj, "source_name")?;
                    LocationField::Coordinate {
                        lat,
                        lng,
                        coord_system,
                        source_name,
                    }
                }
                "fuzzy" => {
                    let name = get_string(doc, payload_obj, "name")?.unwrap_or_default();
                    let place_type =
                        get_string(doc, payload_obj, "place_type")?.unwrap_or_default();
                    LocationField::Fuzzy { name, place_type }
                }
                _ => return Ok(None),
            };
            Ok(Some(ContentEntryPayload::Location(location)))
        }
        "markdown" => {
            let doc_uuid = get_string(doc, payload_obj, "doc_uuid")?.unwrap_or_default();
            Ok(Some(ContentEntryPayload::Markdown { doc_uuid }))
        }
        "json_object" => {
            let data_doc_uuid = get_string(doc, payload_obj, "data_doc_uuid")?.unwrap_or_default();
            let schema_doc_uuid = get_string(doc, payload_obj, "schema_doc_uuid")?;
            Ok(Some(ContentEntryPayload::JsonObject(
                crate::datatype::JsonObjectField {
                    data_doc_uuid,
                    schema_doc_uuid,
                },
            )))
        }
        "date" => {
            let timestamp_ms = get_u64(doc, payload_obj, "timestamp_ms")?.unwrap_or(0) as i64;
            let has_time = match doc.get(payload_obj, "has_time")? {
                Some((Value::Scalar(sv), _)) => match sv.as_ref() {
                    ScalarValue::Boolean(b) => *b,
                    _ => false,
                },
                _ => false,
            };
            let timezone = get_string(doc, payload_obj, "timezone")?;
            Ok(Some(ContentEntryPayload::Date(DateField {
                timestamp_ms,
                has_time,
                timezone,
            })))
        }
        "image" => {
            let uri = get_string(doc, payload_obj, "uri")?.unwrap_or_default();
            let caption = get_string(doc, payload_obj, "caption")?;
            let width = get_u64(doc, payload_obj, "width")?.map(|v| v as u32);
            let height = get_u64(doc, payload_obj, "height")?.map(|v| v as u32);
            let size_bytes = get_u64(doc, payload_obj, "size_bytes")?;
            let device_id = get_string(doc, payload_obj, "device_id")?;
            Ok(Some(ContentEntryPayload::Image(ImageField {
                uri,
                caption,
                width,
                height,
                size_bytes,
                device_id,
            })))
        }
        "url" => {
            let url = get_string(doc, payload_obj, "url")?.unwrap_or_default();
            let title = get_string(doc, payload_obj, "title")?;
            let description = get_string(doc, payload_obj, "description")?;
            let image_url = get_string(doc, payload_obj, "image_url")?;
            let favicon_url = get_string(doc, payload_obj, "favicon_url")?;
            let site_name = get_string(doc, payload_obj, "site_name")?;
            let resolved = match doc.get(payload_obj, "resolved")? {
                Some((Value::Scalar(sv), _)) => match sv.as_ref() {
                    ScalarValue::Boolean(b) => *b,
                    _ => false,
                },
                _ => false,
            };
            Ok(Some(ContentEntryPayload::Url(UrlField {
                url,
                title,
                description,
                image_url,
                favicon_url,
                site_name,
                resolved,
            })))
        }
        "custom" => {
            let data_str = get_string(doc, payload_obj, "data")?.unwrap_or_default();
            let data: serde_json::Value =
                serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null);
            Ok(Some(ContentEntryPayload::Custom {
                content_type: "custom".to_string(),
                data,
            }))
        }
        other => {
            let data_str = get_string(doc, payload_obj, "data")?.unwrap_or_default();
            let data: serde_json::Value = if data_str.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null)
            };
            Ok(Some(ContentEntryPayload::Custom {
                content_type: other.to_string(),
                data,
            }))
        }
    }
}

/// Extract view from a generic thing-owned content document.
pub fn extract_thing_content_view(
    doc_bytes: &[u8],
    document_uuid: &str,
    thing_uuid: &str,
) -> Result<ThingContentView> {
    if doc_bytes.is_empty() {
        return Ok(ThingContentView {
            schema_version: CURRENT_SCHEMA_VERSION,
            document_uuid: document_uuid.to_string(),
            thing_uuid: thing_uuid.to_string(),
            content_type: "markdown".to_string(),
            content: None,
        });
    }

    let doc = AutoCommit::load(doc_bytes).context("Failed to load thing content document")?;
    extract_thing_content_view_from_doc(&doc, document_uuid, thing_uuid)
}

/// Extract view from a generic thing-owned content document (already loaded)
pub fn extract_thing_content_view_from_doc(
    doc: &AutoCommit,
    document_uuid: &str,
    thing_uuid: &str,
) -> Result<ThingContentView> {
    let schema_version = get_u64(doc, &automerge::ROOT, Schema::KEY_SCHEMA_VERSION)?
        .unwrap_or(CURRENT_SCHEMA_VERSION as u64) as u32;

    let stored_document_uuid = get_string(doc, &automerge::ROOT, "document_uuid")?
        .unwrap_or_else(|| document_uuid.to_string());

    let stored_uuid =
        get_string(doc, &automerge::ROOT, "thing_uuid")?.unwrap_or_else(|| thing_uuid.to_string());
    let content_type = get_string(doc, &automerge::ROOT, "content_type")?
        .unwrap_or_else(|| "markdown".to_string());

    // Read content
    let content = if let Some((Value::Object(ObjType::Map), content_obj)) =
        doc.get(automerge::ROOT, Schema::KEY_CONTENT)?
    {
        read_content_from_obj(doc, &content_obj)?
    } else {
        None
    };

    Ok(ThingContentView {
        schema_version,
        document_uuid: stored_document_uuid,
        thing_uuid: stored_uuid,
        content_type,
        content,
    })
}

/// Extract view from a ThingMarkdown document (CrdtDataType::ThingMarkdown)
pub fn extract_thing_markdown_view(
    doc_bytes: &[u8],
    thing_uuid: &str,
) -> Result<ThingMarkdownView> {
    let view = extract_thing_content_view(doc_bytes, thing_uuid, thing_uuid)?;
    Ok(ThingMarkdownView {
        schema_version: view.schema_version,
        thing_uuid: view.thing_uuid,
        content: view.content,
    })
}

/// Extract view from a ThingMarkdown document (already loaded)
pub fn extract_thing_markdown_view_from_doc(
    doc: &AutoCommit,
    thing_uuid: &str,
) -> Result<ThingMarkdownView> {
    let view = extract_thing_content_view_from_doc(doc, thing_uuid, thing_uuid)?;
    Ok(ThingMarkdownView {
        schema_version: view.schema_version,
        thing_uuid: view.thing_uuid,
        content: view.content,
    })
}

/// Read content from a content object (shared between legacy and v3)
fn read_content_from_obj(doc: &AutoCommit, content_obj: &ObjId) -> Result<Option<ContentView>> {
    let kind = get_string(doc, content_obj, "kind")?.unwrap_or_else(|| "markdown".to_string());
    let payload = get_json_string(doc, content_obj, "payload")?;

    let blocks = if let Some((Value::Object(ObjType::List), blocks_list)) =
        doc.get(content_obj, "blocks")?
    {
        let mut out = Vec::new();
        for i in 0..doc.length(&blocks_list) {
            let Some((Value::Object(ObjType::Map), block_obj)) = doc.get(&blocks_list, i)? else {
                continue;
            };
            let id = get_string(doc, &block_obj, "id")?.unwrap_or_default();
            let typ =
                get_string(doc, &block_obj, "type")?.unwrap_or_else(|| "paragraph".to_string());
            let attrs = get_json_string(doc, &block_obj, "attrs")?;
            let text = read_block_text_chars(doc, &block_obj)?;
            out.push(BlockView {
                id,
                r#type: typ,
                attrs,
                text,
            });
        }
        Some(out)
    } else {
        None
    };

    Ok(Some(ContentView {
        kind,
        blocks,
        payload,
    }))
}

/// Helper to read f64 from automerge
fn get_f64(doc: &AutoCommit, obj: &ObjId, key: &str) -> Result<Option<f64>> {
    match doc.get(obj, key)? {
        Some((Value::Scalar(sv), _)) => match sv.as_ref() {
            ScalarValue::F64(f) => Ok(Some(*f)),
            ScalarValue::Int(i) => Ok(Some(*i as f64)),
            ScalarValue::Uint(u) => Ok(Some(*u as f64)),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests_v3 {
    use super::*;
    use crate::schema::Schema;

    #[test]
    fn test_extract_root_view() {
        let doc_bytes = Schema::init_root_doc("test-actor").unwrap();
        let view = extract_root_view(&doc_bytes).unwrap();
        assert_eq!(view.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(view.epoch, 0);
        assert!(view.collection_uuids.is_empty());
    }

    #[test]
    fn test_extract_collection_doc_view() {
        let doc_bytes = Schema::init_collection_doc("test-actor", "coll-123").unwrap();
        let view = extract_collection_doc_view(&doc_bytes, "coll-123").unwrap();
        assert_eq!(view.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(view.meta.id, "coll-123");
        assert_eq!(view.meta.status, "active");
        assert!(view.things.is_empty());
    }

    #[test]
    fn test_extract_thing_markdown_view() {
        let doc_bytes = Schema::init_thing_markdown_doc("test-actor", "thing-456").unwrap();
        let view = extract_thing_markdown_view(&doc_bytes, "thing-456").unwrap();
        assert_eq!(view.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(view.thing_uuid, "thing-456");
        assert!(view.content.is_some());
    }

    #[test]
    fn test_extract_generic_thing_content_view() {
        let doc_bytes =
            Schema::init_thing_content_doc("test-actor", "content-456", "thing-456", "markdown")
                .unwrap();
        let view = extract_thing_content_view(&doc_bytes, "content-456", "thing-456").unwrap();
        assert_eq!(view.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(view.document_uuid, "content-456");
        assert_eq!(view.thing_uuid, "thing-456");
        assert_eq!(view.content_type, "markdown");
        assert!(view.content.is_some());
    }
}
