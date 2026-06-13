use anyhow::{Context, Result};
use automerge::transaction::Transactable;
use automerge::{AutoCommit, ObjId, ObjType, ReadDoc};

use crate::datatype::CrdtDataType;
use crate::util::{ensure_map_key, put_string, put_u64, set_doc_actor};

/// Schema version for the new multi-document architecture.
/// Version 3 introduces document splitting: Root + Collection + Thing-owned content docs.
pub const CURRENT_SCHEMA_VERSION: u32 = 3;

/// Legacy schema version (single-document architecture)
pub const LEGACY_SCHEMA_VERSION: u32 = 2;

pub struct Schema;

impl Schema {
    // Common keys
    pub const KEY_SCHEMA_VERSION: &'static str = "schema_version";
    pub const KEY_EPOCH: &'static str = "epoch";

    // Legacy keys (for migration detection)
    pub const KEY_COLLECTIONS: &'static str = "collections";
    pub const KEY_THINGS: &'static str = "things";

    // Root document keys
    pub const KEY_COLLECTION_UUIDS: &'static str = "collection_uuids";

    // Collection document keys
    pub const KEY_META: &'static str = "meta";
    pub const KEY_THING_MAP: &'static str = "things"; // Map<ThingId, ThingMeta>
    pub const KEY_BUILT_IN: &'static str = "built_in";

    // Thing-owned content document keys
    pub const KEY_CONTENT: &'static str = "content";

    // ========================================================================
    // Legacy single-document init (for backward compatibility during transition)
    // ========================================================================

    /// Ensure root structure for legacy single-document architecture (v2)
    pub fn ensure_root(doc: &mut AutoCommit, actor: &str, epoch: u64) -> Result<(ObjId, ObjId)> {
        set_doc_actor(doc, actor);

        // schema_version
        let needs_schema = match doc.get(automerge::ROOT, Self::KEY_SCHEMA_VERSION)? {
            None => true,
            Some(_) => false,
        };
        if needs_schema {
            doc.put(
                automerge::ROOT,
                Self::KEY_SCHEMA_VERSION,
                i64::try_from(LEGACY_SCHEMA_VERSION).unwrap_or(i64::MAX),
            )
            .context("Failed to set schema_version")?;
        }

        // epoch
        put_u64(doc, &automerge::ROOT, Self::KEY_EPOCH, epoch)?;

        let collections = ensure_map_key(doc, &automerge::ROOT, Self::KEY_COLLECTIONS)
            .context("Failed to ensure collections root map")?;
        let things = ensure_map_key(doc, &automerge::ROOT, Self::KEY_THINGS)
            .context("Failed to ensure things root map")?;
        Ok((collections, things))
    }

    pub fn set_server_checkpoint(doc: &mut AutoCommit, stamp: &str) -> Result<()> {
        put_string(doc, &automerge::ROOT, "server_checkpoint", stamp)
    }

    // ========================================================================
    // New multi-document architecture (v3)
    // ========================================================================

    /// Initialize a new Root document
    ///
    /// Structure:
    /// ```text
    /// {
    ///   schema_version: 3,
    ///   epoch: 0,
    ///   collection_uuids: []  // Automerge list acting as a set
    /// }
    /// ```
    pub fn init_root_doc(actor: &str) -> Result<Vec<u8>> {
        let mut doc = AutoCommit::new();
        set_doc_actor(&mut doc, actor);

        doc.put(
            automerge::ROOT,
            Self::KEY_SCHEMA_VERSION,
            i64::from(CURRENT_SCHEMA_VERSION),
        )
        .context("Failed to set schema_version")?;

        put_u64(&mut doc, &automerge::ROOT, Self::KEY_EPOCH, 0)?;

        // collection_uuids as a list (we'll manage it as a set in application code)
        doc.put_object(automerge::ROOT, Self::KEY_COLLECTION_UUIDS, ObjType::List)
            .context("Failed to create collection_uuids list")?;

        Ok(doc.save())
    }

    /// Initialize a new Collection document
    ///
    /// Structure:
    /// ```text
    /// {
    ///   schema_version: 3,
    ///   meta: {
    ///     title: "",
    ///     status: "active",
    ///     edit_clock: { actor, seq },
    ///     // trigger, tombstone, attrs as needed
    ///   },
    ///   things: {}  // Map<ThingId, ThingMeta>
    /// }
    /// ```
    pub fn init_collection_doc(actor: &str, collection_uuid: &str) -> Result<Vec<u8>> {
        let mut doc = AutoCommit::new();
        set_doc_actor(&mut doc, actor);

        doc.put(
            automerge::ROOT,
            Self::KEY_SCHEMA_VERSION,
            i64::from(CURRENT_SCHEMA_VERSION),
        )
        .context("Failed to set schema_version")?;

        // Meta object
        let meta = doc
            .put_object(automerge::ROOT, Self::KEY_META, ObjType::Map)
            .context("Failed to create meta map")?;

        // Store the collection UUID in meta for reference
        put_string(&mut doc, &meta, "id", collection_uuid)?;
        put_string(&mut doc, &meta, "title", "")?;
        put_string(&mut doc, &meta, "status", "active")?;

        // Initialize edit_clock
        let clock = doc
            .put_object(&meta, "edit_clock", ObjType::Map)
            .context("Failed to create edit_clock")?;
        put_string(&mut doc, &clock, "actor", actor)?;
        put_u64(&mut doc, &clock, "seq", 1)?;

        // Things map
        doc.put_object(automerge::ROOT, Self::KEY_THING_MAP, ObjType::Map)
            .context("Failed to create things map")?;

        Ok(doc.save())
    }

    /// Initialize a new thing-owned content document.
    ///
    /// Structure:
    /// ```text
    /// {
    ///   schema_version: 3,
    ///   document_uuid: "<uuid>",
    ///   thing_uuid: "<uuid>",
    ///   content_type: "markdown",
    ///   content: {
    ///     kind: "markdown",
    ///     blocks: []
    ///   }
    /// }
    /// ```
    pub fn init_thing_content_doc(
        actor: &str,
        document_uuid: &str,
        thing_uuid: &str,
        content_type: &str,
    ) -> Result<Vec<u8>> {
        let mut doc = AutoCommit::new();
        set_doc_actor(&mut doc, actor);

        doc.put(
            automerge::ROOT,
            Self::KEY_SCHEMA_VERSION,
            i64::from(CURRENT_SCHEMA_VERSION),
        )
        .context("Failed to set schema_version")?;

        put_string(&mut doc, &automerge::ROOT, "document_uuid", document_uuid)?;
        // Store thing_uuid for reference
        put_string(&mut doc, &automerge::ROOT, "thing_uuid", thing_uuid)?;
        put_string(&mut doc, &automerge::ROOT, "content_type", content_type)?;

        // Content object
        let content = doc
            .put_object(automerge::ROOT, Self::KEY_CONTENT, ObjType::Map)
            .context("Failed to create content map")?;

        put_string(&mut doc, &content, "kind", content_type)?;

        // Blocks list
        doc.put_object(&content, "blocks", ObjType::List)
            .context("Failed to create blocks list")?;

        Ok(doc.save())
    }

    /// Initialize a new ThingMarkdown document.
    ///
    /// This remains as a compatibility wrapper around the generic thing-content
    /// document initializer while `thing_markdown` is still the only synced
    /// content-document family.
    pub fn init_thing_markdown_doc(actor: &str, thing_uuid: &str) -> Result<Vec<u8>> {
        Self::init_thing_content_doc(actor, thing_uuid, thing_uuid, "markdown")
    }

    /// Check if a document is using the new v3 schema
    pub fn is_v3_doc(doc_bytes: &[u8]) -> bool {
        if doc_bytes.is_empty() {
            return false;
        }
        if let Ok(doc) = AutoCommit::load(doc_bytes) {
            if let Ok(Some((value, _))) = doc.get(automerge::ROOT, Self::KEY_SCHEMA_VERSION) {
                if let Some(v) = value.to_i64() {
                    return v >= i64::from(CURRENT_SCHEMA_VERSION);
                }
            }
        }
        false
    }

    /// Detect document type from bytes (for v3 documents)
    pub fn detect_doc_type(doc_bytes: &[u8]) -> Result<Option<CrdtDataType>> {
        if doc_bytes.is_empty() {
            return Ok(None);
        }

        let doc = AutoCommit::load(doc_bytes).context("Failed to load document")?;

        // Check for Root document marker (collection_uuids list)
        if doc
            .get(automerge::ROOT, Self::KEY_COLLECTION_UUIDS)?
            .is_some()
        {
            return Ok(Some(CrdtDataType::Root));
        }

        // Check for Collection document marker (meta + things map)
        if doc.get(automerge::ROOT, Self::KEY_META)?.is_some() {
            return Ok(Some(CrdtDataType::Collection));
        }

        // Check for thing-owned content document marker (thing_uuid + content)
        if doc.get(automerge::ROOT, "thing_uuid")?.is_some() {
            return Ok(Some(CrdtDataType::ThingMarkdown));
        }

        // Legacy v2 document (has collections + things at root)
        if doc.get(automerge::ROOT, Self::KEY_COLLECTIONS)?.is_some()
            && doc.get(automerge::ROOT, Self::KEY_THINGS)?.is_some()
        {
            return Ok(None); // Legacy document, not a v3 type
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_root_doc() {
        let doc_bytes = Schema::init_root_doc("test-actor").unwrap();
        assert!(Schema::is_v3_doc(&doc_bytes));
        assert_eq!(
            Schema::detect_doc_type(&doc_bytes).unwrap(),
            Some(CrdtDataType::Root)
        );
    }

    #[test]
    fn test_init_collection_doc() {
        let doc_bytes = Schema::init_collection_doc("test-actor", "coll-uuid-123").unwrap();
        assert!(Schema::is_v3_doc(&doc_bytes));
        assert_eq!(
            Schema::detect_doc_type(&doc_bytes).unwrap(),
            Some(CrdtDataType::Collection)
        );
    }

    #[test]
    fn test_init_thing_markdown_doc() {
        let doc_bytes = Schema::init_thing_markdown_doc("test-actor", "thing-uuid-456").unwrap();
        assert!(Schema::is_v3_doc(&doc_bytes));
        assert_eq!(
            Schema::detect_doc_type(&doc_bytes).unwrap(),
            Some(CrdtDataType::ThingMarkdown)
        );
    }

    #[test]
    fn test_init_thing_content_doc() {
        let doc_bytes = Schema::init_thing_content_doc(
            "test-actor",
            "content-uuid-789",
            "thing-uuid-456",
            "markdown",
        )
        .unwrap();
        let doc = AutoCommit::load(&doc_bytes).unwrap();

        assert!(Schema::is_v3_doc(&doc_bytes));
        assert_eq!(
            Schema::detect_doc_type(&doc_bytes).unwrap(),
            Some(CrdtDataType::ThingMarkdown)
        );
        assert_eq!(
            doc.get(automerge::ROOT, "document_uuid")
                .unwrap()
                .unwrap()
                .0
                .to_str()
                .unwrap(),
            "content-uuid-789"
        );
        assert_eq!(
            doc.get(automerge::ROOT, "content_type")
                .unwrap()
                .unwrap()
                .0
                .to_str()
                .unwrap(),
            "markdown"
        );
    }
}
