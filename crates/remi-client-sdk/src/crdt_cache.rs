use crate::things_crdt::{DocumentKey, DocumentState, ThingsDocumentSet};
use remi_things_crdt::CrdtDataType;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

const GLOBAL_DOCUMENT_SET_CACHE_CAPACITY: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    namespace: String,
    device_id: String,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    revision: u64,
    doc_set: ThingsDocumentSet,
}

#[derive(Default)]
struct GlobalDocumentSetCache {
    entries: HashMap<CacheKey, CacheEntry>,
    lru: VecDeque<CacheKey>,
}

impl GlobalDocumentSetCache {
    fn touch(&mut self, key: &CacheKey) {
        if let Some(index) = self.lru.iter().position(|existing| existing == key) {
            self.lru.remove(index);
        }
        self.lru.push_front(key.clone());
    }

    fn evict_if_needed(&mut self) {
        while self.entries.len() > GLOBAL_DOCUMENT_SET_CACHE_CAPACITY {
            let Some(key) = self.lru.pop_back() else {
                break;
            };
            self.entries.remove(&key);
        }
    }

    fn get(
        &mut self,
        namespace: &str,
        device_id: &str,
        revision: u64,
    ) -> Option<ThingsDocumentSet> {
        let key = CacheKey {
            namespace: namespace.to_string(),
            device_id: device_id.to_string(),
        };
        let entry = self.entries.get(&key)?.clone();
        if entry.revision != revision {
            self.entries.remove(&key);
            self.lru.retain(|existing| existing != &key);
            return None;
        }

        self.touch(&key);
        Some(entry.doc_set)
    }

    fn put(
        &mut self,
        namespace: String,
        device_id: String,
        revision: u64,
        doc_set: ThingsDocumentSet,
    ) {
        let key = CacheKey {
            namespace,
            device_id,
        };
        self.entries
            .insert(key.clone(), CacheEntry { revision, doc_set });
        self.touch(&key);
        self.evict_if_needed();
    }

    fn invalidate_namespace_revision_older_than(&mut self, namespace: &str, min_revision: u64) {
        self.entries
            .retain(|key, entry| !(key.namespace == namespace && entry.revision < min_revision));
        self.lru.retain(|key| self.entries.contains_key(key));
    }

    fn upsert_document(
        &mut self,
        namespace: &str,
        revision: u64,
        key: &DocumentKey,
        state: &DocumentState,
    ) {
        let matching_keys: Vec<CacheKey> = self
            .entries
            .keys()
            .filter(|cache_key| cache_key.namespace == namespace)
            .cloned()
            .collect();

        for cache_key in matching_keys {
            if let Some(entry) = self.entries.get_mut(&cache_key) {
                if entry.revision > revision {
                    continue;
                }
                entry.doc_set.set(key.clone(), state.clone());
                entry.revision = revision;
            }
            self.touch(&cache_key);
        }
    }

    fn remove_document(&mut self, namespace: &str, revision: u64, key: &DocumentKey) {
        let matching_keys: Vec<CacheKey> = self
            .entries
            .keys()
            .filter(|cache_key| cache_key.namespace == namespace)
            .cloned()
            .collect();

        for cache_key in matching_keys {
            if let Some(entry) = self.entries.get_mut(&cache_key) {
                if entry.revision > revision {
                    continue;
                }
                entry.doc_set.remove_document(key);
                entry.revision = revision;
            }
            self.touch(&cache_key);
        }
    }
}

fn global_cache() -> &'static Mutex<GlobalDocumentSetCache> {
    static CACHE: OnceLock<Mutex<GlobalDocumentSetCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(GlobalDocumentSetCache::default()))
}

pub(crate) fn get(namespace: &str, device_id: &str, revision: u64) -> Option<ThingsDocumentSet> {
    global_cache()
        .lock()
        .expect("global document set cache mutex poisoned")
        .get(namespace, device_id, revision)
}

pub(crate) fn put(namespace: String, device_id: String, revision: u64, doc_set: ThingsDocumentSet) {
    global_cache()
        .lock()
        .expect("global document set cache mutex poisoned")
        .put(namespace, device_id, revision, doc_set);
}

pub(crate) fn invalidate_namespace_revision_older_than(namespace: &str, min_revision: u64) {
    global_cache()
        .lock()
        .expect("global document set cache mutex poisoned")
        .invalidate_namespace_revision_older_than(namespace, min_revision);
}

pub(crate) fn upsert_document(
    namespace: &str,
    revision: u64,
    uuid: &str,
    data_type: &str,
    automerge_doc: &[u8],
    sync_state: &[u8],
    dirty: bool,
    last_sync_at: Option<&str>,
) {
    let Some(data_type) = parse_data_type(data_type) else {
        return;
    };

    global_cache()
        .lock()
        .expect("global document set cache mutex poisoned")
        .upsert_document(
            namespace,
            revision,
            &DocumentKey {
                uuid: uuid.to_string(),
                data_type,
            },
            &DocumentState {
                automerge_doc: automerge_doc.to_vec(),
                sync_state: sync_state.to_vec(),
                dirty,
                last_sync_at: last_sync_at.map(str::to_string),
            },
        );
}

pub(crate) fn remove_document(namespace: &str, revision: u64, uuid: &str, data_type: &str) {
    let Some(data_type) = parse_data_type(data_type) else {
        return;
    };

    global_cache()
        .lock()
        .expect("global document set cache mutex poisoned")
        .remove_document(
            namespace,
            revision,
            &DocumentKey {
                uuid: uuid.to_string(),
                data_type,
            },
        );
}

fn parse_data_type(data_type: &str) -> Option<CrdtDataType> {
    match data_type {
        "root" => Some(CrdtDataType::Root),
        "collection" => Some(CrdtDataType::Collection),
        "thing_markdown" => Some(CrdtDataType::ThingMarkdown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidating_namespace_revision_evicts_only_stale_matching_entries() {
        let mut cache = GlobalDocumentSetCache::default();
        cache.put(
            "db-a".to_string(),
            "device-a".to_string(),
            1,
            ThingsDocumentSet::new("device-a"),
        );
        cache.put(
            "db-a".to_string(),
            "device-b".to_string(),
            2,
            ThingsDocumentSet::new("device-b"),
        );
        cache.put(
            "db-b".to_string(),
            "device-b".to_string(),
            1,
            ThingsDocumentSet::new("device-b"),
        );

        cache.invalidate_namespace_revision_older_than("db-a", 2);

        assert!(cache.get("db-a", "device-a", 1).is_none());
        assert!(cache.get("db-a", "device-b", 2).is_some());
        assert!(cache.get("db-b", "device-b", 1).is_some());
    }

    #[test]
    fn upserting_document_updates_only_matching_namespace_entries() {
        let mut cache = GlobalDocumentSetCache::default();

        let mut doc_set_a = ThingsDocumentSet::new("device-a");
        doc_set_a.set(
            DocumentKey::collection("coll-1"),
            DocumentState {
                automerge_doc: vec![1],
                sync_state: vec![2],
                dirty: false,
                last_sync_at: None,
            },
        );
        cache.put("db-a".to_string(), "device-a".to_string(), 1, doc_set_a);

        let mut doc_set_b = ThingsDocumentSet::new("device-b");
        doc_set_b.set(
            DocumentKey::collection("coll-1"),
            DocumentState {
                automerge_doc: vec![9],
                sync_state: vec![9],
                dirty: false,
                last_sync_at: None,
            },
        );
        cache.put("db-b".to_string(), "device-b".to_string(), 1, doc_set_b);

        let key = DocumentKey::collection("coll-1");
        let state = DocumentState {
            automerge_doc: vec![3],
            sync_state: vec![4],
            dirty: true,
            last_sync_at: Some("x".to_string()),
        };
        cache.upsert_document("db-a", 2, &key, &state);

        let updated = cache.get("db-a", "device-a", 2).expect("db-a cache hit");
        assert_eq!(
            updated.get(&key).expect("updated doc").automerge_doc,
            vec![3]
        );

        let untouched = cache.get("db-b", "device-b", 1).expect("db-b cache hit");
        assert_eq!(
            untouched.get(&key).expect("untouched doc").automerge_doc,
            vec![9]
        );
    }

    #[test]
    fn removing_document_updates_only_matching_namespace_entries() {
        let mut cache = GlobalDocumentSetCache::default();
        let key = DocumentKey::collection("coll-1");

        let mut doc_set_a = ThingsDocumentSet::new("device-a");
        doc_set_a.set(key.clone(), DocumentState::new_empty());
        cache.put("db-a".to_string(), "device-a".to_string(), 1, doc_set_a);

        let mut doc_set_b = ThingsDocumentSet::new("device-b");
        doc_set_b.set(key.clone(), DocumentState::new_empty());
        cache.put("db-b".to_string(), "device-b".to_string(), 1, doc_set_b);

        cache.remove_document("db-a", 2, &key);

        let updated = cache.get("db-a", "device-a", 2).expect("db-a cache hit");
        assert!(updated.get(&key).is_none());

        let untouched = cache.get("db-b", "device-b", 1).expect("db-b cache hit");
        assert!(untouched.get(&key).is_some());
    }
}
