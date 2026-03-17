use chrono::{DateTime, Duration, Utc};
use remi_agentloop::prelude::AgentError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

const FEATURED_SKILL_LIMIT: usize = 8;
const FEATURED_CACHE_TTL_MINUTES: i64 = 30;
const SKILL_INDEX_FILE: &str = ".skill-index.json";

// ── SkillStore trait ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinSkill {
    pub name: &'static str,
    pub description: &'static str,
    pub content: &'static str,
}

impl BuiltinSkill {
    fn summary(&self) -> SkillSummary {
        SkillSummary {
            name: self.name.to_string(),
            description: Some(self.description.to_string()),
        }
    }
}

/// Persistent backend for named skill documents.
#[allow(async_fn_in_trait)]
pub trait SkillStore: Send + Sync + 'static {
    /// Save (create or overwrite) a skill. Returns the storage path/key.
    async fn save(&self, name: &str, content: &str) -> Result<String, AgentError>;
    /// Retrieve a skill's content. Returns `None` if the skill doesn't exist.
    /// A successful retrieval updates the skill's recent-use timestamp.
    async fn get(&self, name: &str) -> Result<Option<String>, AgentError>;
    /// Search skills by one or more keywords across skill names and descriptions.
    async fn search(&self, query: &str) -> Result<Vec<SkillSummary>, AgentError>;
    /// Delete a skill. Returns `true` if it existed.
    async fn delete(&self, name: &str) -> Result<bool, AgentError>;
    /// Return the cached featured-skill snapshot used by prompt injection.
    fn featured_summaries(&self) -> Vec<SkillSummary>;
}

pub struct BuiltinSkillStore<S> {
    inner: S,
    builtins: BTreeMap<String, BuiltinSkill>,
}

impl<S> BuiltinSkillStore<S> {
    pub fn new(inner: S, builtins: impl IntoIterator<Item = BuiltinSkill>) -> Self {
        let builtins = builtins
            .into_iter()
            .map(|skill| (skill.name.to_string(), skill))
            .collect();
        Self { inner, builtins }
    }

    fn builtin(&self, name: &str) -> Option<&BuiltinSkill> {
        if let Some(skill) = self.builtins.get(name) {
            return Some(skill);
        }

        let lookup = sanitize_skill_name(name);
        self.builtins
            .values()
            .find(|skill| sanitize_skill_name(skill.name) == lookup)
    }

    fn builtin_matches(&self, query: &str) -> Vec<SkillSummary> {
        let keywords = parse_keywords(query);
        if keywords.is_empty() {
            return Vec::new();
        }

        let mut matches: Vec<(usize, SkillSummary)> = self
            .builtins
            .values()
            .filter_map(|skill| {
                let hit_count = builtin_keyword_hit_count(skill, &keywords);
                (hit_count > 0).then_some((hit_count, skill.summary()))
            })
            .collect();

        matches.sort_by(|(hits_a, summary_a), (hits_b, summary_b)| {
            hits_b
                .cmp(hits_a)
                .then_with(|| summary_a.name.cmp(&summary_b.name))
        });

        matches.into_iter().map(|(_, summary)| summary).collect()
    }
}

impl<S: SkillStore> SkillStore for BuiltinSkillStore<S> {
    async fn save(&self, name: &str, content: &str) -> Result<String, AgentError> {
        if self.builtin(name).is_some() {
            return Err(AgentError::other(format!(
                "skill '{name}' is builtin and read-only"
            )));
        }
        self.inner.save(name, content).await
    }

    async fn get(&self, name: &str) -> Result<Option<String>, AgentError> {
        if let Some(skill) = self.builtin(name) {
            return Ok(Some(skill.content.to_string()));
        }
        self.inner.get(name).await
    }

    async fn search(&self, query: &str) -> Result<Vec<SkillSummary>, AgentError> {
        let mut results = self.builtin_matches(query);
        let mut seen: HashSet<String> = results.iter().map(|entry| entry.name.clone()).collect();

        for entry in self.inner.search(query).await? {
            if seen.insert(entry.name.clone()) {
                results.push(entry);
            }
        }

        Ok(results)
    }

    async fn delete(&self, name: &str) -> Result<bool, AgentError> {
        if self.builtin(name).is_some() {
            return Err(AgentError::other(format!(
                "skill '{name}' is builtin and read-only"
            )));
        }
        self.inner.delete(name).await
    }

    fn featured_summaries(&self) -> Vec<SkillSummary> {
        let mut featured = Vec::new();
        let mut seen = HashSet::new();

        for skill in self.builtins.values() {
            let summary = skill.summary();
            seen.insert(summary.name.clone());
            featured.push(summary);
        }

        for summary in self.inner.featured_summaries() {
            if seen.insert(summary.name.clone()) {
                featured.push(summary);
            }
            if featured.len() >= FEATURED_SKILL_LIMIT {
                break;
            }
        }

        featured.truncate(FEATURED_SKILL_LIMIT);
        featured
    }
}

// ── Shared metadata/index helpers ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SkillMetadata {
    name: String,
    description: Option<String>,
    updated_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct SkillIndexDisk {
    entries: BTreeMap<String, SkillMetadata>,
}

#[derive(Debug, Clone)]
struct FeaturedCache {
    generated_at: DateTime<Utc>,
    summaries: Vec<SkillSummary>,
}

#[derive(Debug, Default)]
struct SkillIndexState {
    entries: BTreeMap<String, SkillMetadata>,
    featured_cache: Option<FeaturedCache>,
}

impl SkillIndexState {
    fn snapshot(&self) -> SkillIndexDisk {
        SkillIndexDisk {
            entries: self.entries.clone(),
        }
    }

    fn featured_summaries(&mut self, now: DateTime<Utc>) -> Vec<SkillSummary> {
        if let Some(cache) = &self.featured_cache {
            if now.signed_duration_since(cache.generated_at)
                < Duration::minutes(FEATURED_CACHE_TTL_MINUTES)
            {
                return cache.summaries.clone();
            }
        }

        let mut entries: Vec<_> = self.entries.values().cloned().collect();
        entries.sort_by(|a, b| {
            b.last_used_at
                .cmp(&a.last_used_at)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
                .then_with(|| a.name.cmp(&b.name))
        });

        let summaries: Vec<_> = entries
            .into_iter()
            .take(FEATURED_SKILL_LIMIT)
            .map(|entry| SkillSummary {
                name: entry.name,
                description: entry.description,
            })
            .collect();

        self.featured_cache = Some(FeaturedCache {
            generated_at: now,
            summaries: summaries.clone(),
        });

        summaries
    }

    fn search(&self, query: &str) -> Vec<SkillSummary> {
        let keywords = parse_keywords(query);
        if keywords.is_empty() {
            return Vec::new();
        }

        let mut matches: Vec<(usize, SkillMetadata)> = self
            .entries
            .values()
            .filter_map(|entry| {
                let hit_count = keyword_hit_count(entry, &keywords);
                (hit_count > 0).then_some((hit_count, entry.clone()))
            })
            .collect();

        matches.sort_by(|(hits_a, entry_a), (hits_b, entry_b)| {
            hits_b
                .cmp(hits_a)
                .then_with(|| entry_b.last_used_at.cmp(&entry_a.last_used_at))
                .then_with(|| entry_b.updated_at.cmp(&entry_a.updated_at))
                .then_with(|| entry_a.name.cmp(&entry_b.name))
        });

        matches
            .into_iter()
            .map(|(_, entry)| SkillSummary {
                name: entry.name,
                description: entry.description,
            })
            .collect()
    }

    fn upsert_skill(&mut self, name_hint: &str, content: &str, updated_at: DateTime<Utc>) {
        let summary = parse_skill_summary(name_hint, content);
        let existing_key = self.resolve_lookup_name(name_hint);
        let existing_last_used = existing_key
            .as_ref()
            .and_then(|key| self.entries.get(key))
            .and_then(|entry| entry.last_used_at);

        if let Some(key) = existing_key {
            if key != summary.name {
                self.entries.remove(&key);
            }
        }

        self.entries.insert(
            summary.name.clone(),
            SkillMetadata {
                name: summary.name,
                description: summary.description,
                updated_at,
                last_used_at: existing_last_used,
            },
        );
        self.featured_cache = None;
    }

    fn mark_used_with_timestamp(
        &mut self,
        name_hint: &str,
        content: &str,
        updated_at: DateTime<Utc>,
        used_at: DateTime<Utc>,
    ) {
        let summary = parse_skill_summary(name_hint, content);
        let existing_key = self.resolve_lookup_name(name_hint);

        if let Some(key) = existing_key {
            if key != summary.name {
                self.entries.remove(&key);
            }
        }

        let preserved_updated_at = self
            .entries
            .get(&summary.name)
            .map(|entry| entry.updated_at)
            .unwrap_or(updated_at);

        self.entries.insert(
            summary.name.clone(),
            SkillMetadata {
                name: summary.name,
                description: summary.description,
                updated_at: preserved_updated_at.max(updated_at),
                last_used_at: Some(used_at),
            },
        );
    }

    fn remove_skill(&mut self, name: &str) -> bool {
        let Some(key) = self.resolve_lookup_name(name) else {
            return false;
        };
        self.featured_cache = None;
        self.entries.remove(&key).is_some()
    }

    fn resolve_lookup_name(&self, name: &str) -> Option<String> {
        if self.entries.contains_key(name) {
            return Some(name.to_string());
        }

        let lookup = sanitize_skill_name(name);
        self.entries
            .keys()
            .find(|entry_name| sanitize_skill_name(entry_name) == lookup)
            .cloned()
    }
}

fn parse_keywords(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut keywords = Vec::new();

    for raw in query.split_whitespace() {
        let keyword = raw
            .trim_matches(|ch: char| !ch.is_alphanumeric() && ch != '-' && ch != '_')
            .to_ascii_lowercase();
        if !keyword.is_empty() && seen.insert(keyword.clone()) {
            keywords.push(keyword);
        }
    }

    if keywords.is_empty() {
        let fallback = query.trim().to_ascii_lowercase();
        if !fallback.is_empty() {
            keywords.push(fallback);
        }
    }

    keywords
}

fn keyword_hit_count(entry: &SkillMetadata, keywords: &[String]) -> usize {
    let name = entry.name.to_ascii_lowercase();
    let description = entry
        .description
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();

    keywords
        .iter()
        .filter(|keyword| name.contains(keyword.as_str()) || description.contains(keyword.as_str()))
        .count()
}

fn builtin_keyword_hit_count(skill: &BuiltinSkill, keywords: &[String]) -> usize {
    let name = skill.name.to_ascii_lowercase();
    let description = skill.description.to_ascii_lowercase();

    keywords
        .iter()
        .filter(|keyword| name.contains(keyword.as_str()) || description.contains(keyword.as_str()))
        .count()
}

fn parse_skill_summary(name_hint: &str, content: &str) -> SkillSummary {
    let mut parsed_name = None;
    let mut parsed_description = None;

    if let Some(frontmatter) = extract_frontmatter(content) {
        for line in frontmatter.lines() {
            let Some((raw_key, raw_value)) = line.split_once(':') else {
                continue;
            };
            match raw_key.trim() {
                "name" => {
                    let value = normalize_frontmatter_value(raw_value);
                    if !value.is_empty() {
                        parsed_name = Some(value);
                    }
                }
                "description" => {
                    let value = normalize_frontmatter_value(raw_value);
                    if !value.is_empty() {
                        parsed_description = Some(value);
                    }
                }
                _ => {}
            }
        }
    }

    SkillSummary {
        name: parsed_name.unwrap_or_else(|| name_hint.to_string()),
        description: parsed_description,
    }
}

fn extract_frontmatter(content: &str) -> Option<&str> {
    if !content.starts_with("---") {
        return None;
    }
    let rest = content["---".len()..].trim_start_matches('\n');
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn normalize_frontmatter_value(value: &str) -> String {
    let trimmed = value.trim();
    let unquoted = if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    unquoted.trim().to_string()
}

fn sanitize_skill_name(name: &str) -> String {
    name.replace(['/', '\\', '.'], "_")
}

fn modified_at_or_now(metadata: &std::fs::Metadata) -> DateTime<Utc> {
    metadata
        .modified()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now())
}

fn read_index_sync(index_path: &Path) -> SkillIndexDisk {
    std::fs::read_to_string(index_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn persist_index_sync(index_path: &Path, disk: &SkillIndexDisk) {
    if let Some(parent) = index_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(disk) {
        let _ = std::fs::write(index_path, text);
    }
}

fn rebuild_skill_index(base_dir: &Path, persisted: &SkillIndexDisk) -> SkillIndexDisk {
    let mut entries = BTreeMap::new();
    let Ok(read_dir) = std::fs::read_dir(base_dir) else {
        return SkillIndexDisk { entries };
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };

        let name_hint = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        let summary = parse_skill_summary(name_hint, &content);
        let metadata = entry.metadata().ok();
        let modified_at = metadata
            .as_ref()
            .map(modified_at_or_now)
            .unwrap_or_else(Utc::now);

        let previous = persisted.entries.get(&summary.name).or_else(|| {
            persisted
                .entries
                .values()
                .find(|item| sanitize_skill_name(&item.name) == sanitize_skill_name(name_hint))
        });

        let updated_at = previous
            .map(|item| item.updated_at.max(modified_at))
            .unwrap_or(modified_at);
        let last_used_at = previous.and_then(|item| item.last_used_at);

        entries.insert(
            summary.name.clone(),
            SkillMetadata {
                name: summary.name,
                description: summary.description,
                updated_at,
                last_used_at,
            },
        );
    }

    SkillIndexDisk { entries }
}

async fn persist_index_async(index_path: PathBuf, disk: SkillIndexDisk) -> Result<(), AgentError> {
    if let Some(parent) = index_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
    }
    let text = serde_json::to_string_pretty(&disk)
        .map_err(|e| AgentError::other(format!("skill index serialise: {e}")))?;
    tokio::fs::write(index_path, text)
        .await
        .map_err(|e| AgentError::Io(e.to_string()))
}

// ── FileSkillStore ───────────────────────────────────────────────────────────

/// Stores each skill as `<base_dir>/<name>.md` and keeps a lightweight JSON index.
pub struct FileSkillStore {
    base_dir: PathBuf,
    state: RwLock<SkillIndexState>,
}

impl FileSkillStore {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        let base_dir = base_dir.into();
        let index_path = base_dir.join(SKILL_INDEX_FILE);
        let persisted = read_index_sync(&index_path);
        let rebuilt = rebuild_skill_index(&base_dir, &persisted);
        if rebuilt != persisted {
            persist_index_sync(&index_path, &rebuilt);
        }

        Self {
            base_dir,
            state: RwLock::new(SkillIndexState {
                entries: rebuilt.entries,
                featured_cache: None,
            }),
        }
    }

    fn skill_path(&self, name: &str) -> PathBuf {
        self.base_dir
            .join(format!("{}.md", sanitize_skill_name(name)))
    }

    fn index_path(&self) -> PathBuf {
        self.base_dir.join(SKILL_INDEX_FILE)
    }
}

impl SkillStore for FileSkillStore {
    async fn save(&self, name: &str, content: &str) -> Result<String, AgentError> {
        tokio::fs::create_dir_all(&self.base_dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let path = self.skill_path(name);
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;

        let updated_at = Utc::now();
        let disk = {
            let mut state = self.state.write().unwrap();
            state.upsert_skill(name, content, updated_at);
            state.snapshot()
        };
        persist_index_async(self.index_path(), disk).await?;

        Ok(path.to_string_lossy().to_string())
    }

    async fn get(&self, name: &str) -> Result<Option<String>, AgentError> {
        let path = self.skill_path(name);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(AgentError::Io(e.to_string())),
        };

        let updated_at = tokio::fs::metadata(&path)
            .await
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(Utc::now);
        let used_at = Utc::now();
        let disk = {
            let mut state = self.state.write().unwrap();
            state.mark_used_with_timestamp(name, &content, updated_at, used_at);
            state.snapshot()
        };

        if let Err(error) = persist_index_async(self.index_path(), disk).await {
            tracing::warn!(skill = %name, "failed to persist skill recency: {error}");
        }

        Ok(Some(content))
    }

    async fn search(&self, query: &str) -> Result<Vec<SkillSummary>, AgentError> {
        Ok(self.state.read().unwrap().search(query))
    }

    async fn delete(&self, name: &str) -> Result<bool, AgentError> {
        let path = self.skill_path(name);
        let removed_file = match tokio::fs::remove_file(&path).await {
            Ok(()) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(AgentError::Io(e.to_string())),
        };

        let (removed_index, disk) = {
            let mut state = self.state.write().unwrap();
            let removed_index = state.remove_skill(name);
            let disk = state.snapshot();
            (removed_index, disk)
        };

        if removed_file || removed_index {
            persist_index_async(self.index_path(), disk).await?;
        }

        Ok(removed_file || removed_index)
    }

    fn featured_summaries(&self) -> Vec<SkillSummary> {
        self.state.write().unwrap().featured_summaries(Utc::now())
    }
}

// ── InMemorySkillStore ───────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct InMemoryState {
    contents: HashMap<String, String>,
    index: SkillIndexState,
}

/// In-memory skill store for testing.
#[derive(Default)]
pub struct InMemorySkillStore {
    state: Mutex<InMemoryState>,
}

impl InMemorySkillStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SkillStore for InMemorySkillStore {
    async fn save(&self, name: &str, content: &str) -> Result<String, AgentError> {
        let mut state = self.state.lock().unwrap();
        state.contents.insert(name.to_string(), content.to_string());
        state.index.upsert_skill(name, content, Utc::now());
        Ok(format!("memory:{name}"))
    }

    async fn get(&self, name: &str) -> Result<Option<String>, AgentError> {
        let mut state = self.state.lock().unwrap();
        let lookup_name = state
            .index
            .resolve_lookup_name(name)
            .unwrap_or_else(|| name.to_string());
        let Some(content) = state.contents.get(&lookup_name).cloned() else {
            return Ok(None);
        };
        state
            .index
            .mark_used_with_timestamp(&lookup_name, &content, Utc::now(), Utc::now());
        Ok(Some(content))
    }

    async fn search(&self, query: &str) -> Result<Vec<SkillSummary>, AgentError> {
        Ok(self.state.lock().unwrap().index.search(query))
    }

    async fn delete(&self, name: &str) -> Result<bool, AgentError> {
        let mut state = self.state.lock().unwrap();
        let lookup_name = state
            .index
            .resolve_lookup_name(name)
            .unwrap_or_else(|| name.to_string());
        let removed_data = state.contents.remove(&lookup_name).is_some();
        let removed_index = state.index.remove_skill(name);
        Ok(removed_data || removed_index)
    }

    fn featured_summaries(&self) -> Vec<SkillSummary> {
        self.state
            .lock()
            .unwrap()
            .index
            .featured_summaries(Utc::now())
    }
}

#[cfg(test)]
mod tests {
    use super::{BuiltinSkill, BuiltinSkillStore, FileSkillStore, InMemorySkillStore, SkillStore};
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    fn temp_skill_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("remi-skill-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn file_store_rebuilds_index_from_frontmatter() {
        let dir = temp_skill_dir();
        std::fs::write(
            dir.join("rust_check.md"),
            "---\nname: rust-check\ndescription: Verify the Rust workspace\n---\n\nRun cargo test.",
        )
        .unwrap();

        let store = FileSkillStore::new(&dir);
        let results = store.search("rust").await.unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "rust-check");
        assert_eq!(
            results[0].description.as_deref(),
            Some("Verify the Rust workspace")
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn successful_get_updates_recency_without_busting_cache() {
        let store = InMemorySkillStore::new();
        store
            .save(
                "alpha",
                "---\nname: alpha\ndescription: First skill\n---\n\nAlpha body",
            )
            .await
            .unwrap();
        store
            .save(
                "beta",
                "---\nname: beta\ndescription: Second skill\n---\n\nBeta body",
            )
            .await
            .unwrap();

        let first_snapshot = store.featured_summaries();
        assert_eq!(first_snapshot[0].name, "beta");

        let _ = store.get("alpha").await.unwrap();
        let cached_snapshot = store.featured_summaries();
        assert_eq!(cached_snapshot[0].name, "beta");

        {
            let mut state = store.state.lock().unwrap();
            state.index.featured_cache.as_mut().unwrap().generated_at =
                Utc::now() - Duration::minutes(31);
        }

        let refreshed_snapshot = store.featured_summaries();
        assert_eq!(refreshed_snapshot[0].name, "alpha");
    }

    #[tokio::test]
    async fn search_uses_or_matching_and_ranks_by_hit_count() {
        let store = InMemorySkillStore::new();
        store
            .save(
                "rust-sender-map",
                "---\nname: rust-sender-map\ndescription: Map Rust sender identities\n---\n\nBody",
            )
            .await
            .unwrap();
        store
            .save(
                "rust-build",
                "---\nname: rust-build\ndescription: Build the Rust workspace\n---\n\nBody",
            )
            .await
            .unwrap();
        store
            .save(
                "feishu-sender",
                "---\nname: feishu-sender\ndescription: Lookup sender profile in Feishu\n---\n\nBody",
            )
            .await
            .unwrap();

        let results = store.search("rust sender").await.unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].name, "rust-sender-map");
        assert!(results.iter().any(|entry| entry.name == "rust-build"));
        assert!(results.iter().any(|entry| entry.name == "feishu-sender"));
    }

    #[tokio::test]
    async fn builtin_skills_are_searchable_and_featured() {
        let inner = InMemorySkillStore::new();
        inner
            .save(
                "rust-build",
                "---\nname: rust-build\ndescription: Build the Rust workspace\n---\n\nBody",
            )
            .await
            .unwrap();

        let store = BuiltinSkillStore::new(
            inner,
            [BuiltinSkill {
                name: "trigger",
                description: "Builtin trigger capability reference",
                content: "trigger body",
            }],
        );

        let results = store.search("trigger").await.unwrap();
        assert_eq!(results[0].name, "trigger");
        assert_eq!(
            store.get("trigger").await.unwrap().as_deref(),
            Some("trigger body")
        );

        let featured = store.featured_summaries();
        assert_eq!(featured[0].name, "trigger");
        assert!(featured.iter().any(|entry| entry.name == "rust-build"));
    }

    #[tokio::test]
    async fn builtin_skill_names_are_reserved() {
        let store = BuiltinSkillStore::new(
            InMemorySkillStore::new(),
            [BuiltinSkill {
                name: "trigger",
                description: "Builtin trigger capability reference",
                content: "trigger body",
            }],
        );

        let save_error = store
            .save("trigger", "---\nname: trigger\n---\n\nBody")
            .await
            .expect_err("saving builtin skills should fail");
        assert!(save_error.to_string().contains("builtin and read-only"));

        let delete_error = store
            .delete("trigger")
            .await
            .expect_err("deleting builtin skills should fail");
        assert!(delete_error.to_string().contains("builtin and read-only"));
    }
}
