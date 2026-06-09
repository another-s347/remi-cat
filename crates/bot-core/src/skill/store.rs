use remi_agentloop::prelude::AgentError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

const SKILL_FILE_NAME: &str = "SKILL.md";
const PRIMARY_SOURCE: &str = ".remi-cat/skills";
const COMPAT_SOURCE: &str = ".agents/skills";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDocument {
    pub name: String,
    pub description: String,
    pub content: String,
    pub source: String,
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillResource {
    pub skill: String,
    pub path: String,
    pub content: Option<String>,
    pub size: u64,
    pub binary: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinSkill {
    pub name: &'static str,
    pub description: &'static str,
    pub content: &'static str,
}

impl BuiltinSkill {
    fn document(&self) -> Result<SkillDocument, AgentError> {
        let parsed = parse_skill_markdown(self.name, self.content)?;
        Ok(SkillDocument {
            name: parsed.name,
            description: parsed.description,
            content: self.content.to_string(),
            source: "builtin".to_string(),
            root: None,
        })
    }
}

#[allow(async_fn_in_trait)]
pub trait SkillStore: Send + Sync + 'static {
    async fn get(&self, name: &str) -> Result<Option<SkillDocument>, AgentError>;
    async fn search(&self, query: &str) -> Result<Vec<SkillSummary>, AgentError>;
    async fn read_resource(&self, skill: &str, path: &str) -> Result<SkillResource, AgentError>;
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
            .map(|skill| (canonical_name(skill.name), skill))
            .collect();
        Self { inner, builtins }
    }

    fn builtin(&self, name: &str) -> Option<&BuiltinSkill> {
        self.builtins.get(&canonical_name(name))
    }

    fn builtin_summaries(&self) -> Vec<SkillSummary> {
        self.builtins
            .values()
            .map(|skill| SkillSummary {
                name: skill.name.to_string(),
                description: skill.description.to_string(),
                source: "builtin".to_string(),
            })
            .collect()
    }
}

impl<S: SkillStore> SkillStore for BuiltinSkillStore<S> {
    async fn get(&self, name: &str) -> Result<Option<SkillDocument>, AgentError> {
        if let Some(skill) = self.builtin(name) {
            return skill.document().map(Some);
        }
        self.inner.get(name).await
    }

    async fn search(&self, query: &str) -> Result<Vec<SkillSummary>, AgentError> {
        let keywords = parse_keywords(query);
        let mut results = rank_summaries(self.builtin_summaries(), &keywords);
        let mut seen: HashSet<String> = results
            .iter()
            .map(|entry| canonical_name(&entry.name))
            .collect();

        for entry in self.inner.search(query).await? {
            if seen.insert(canonical_name(&entry.name)) {
                results.push(entry);
            }
        }

        Ok(results)
    }

    async fn read_resource(&self, skill: &str, path: &str) -> Result<SkillResource, AgentError> {
        if self.builtin(skill).is_some() {
            return Err(AgentError::tool(
                "skill__read_resource",
                "builtin skills do not have external resources",
            ));
        }
        self.inner.read_resource(skill, path).await
    }

    fn featured_summaries(&self) -> Vec<SkillSummary> {
        let mut featured = self.builtin_summaries();
        let mut seen: HashSet<String> = featured
            .iter()
            .map(|entry| canonical_name(&entry.name))
            .collect();

        for summary in self.inner.featured_summaries() {
            if seen.insert(canonical_name(&summary.name)) {
                featured.push(summary);
            }
        }

        featured
    }
}

#[derive(Debug, Clone)]
struct SkillEntry {
    summary: SkillSummary,
    path: PathBuf,
    root: PathBuf,
}

pub struct FileSkillStore {
    roots: Vec<(PathBuf, String)>,
    entries: RwLock<BTreeMap<String, SkillEntry>>,
}

impl FileSkillStore {
    pub fn new(primary_dir: impl Into<PathBuf>) -> Self {
        Self::with_roots([
            (primary_dir.into(), PRIMARY_SOURCE.to_string()),
            (PathBuf::from(COMPAT_SOURCE), COMPAT_SOURCE.to_string()),
        ])
    }

    pub fn with_roots(roots: impl IntoIterator<Item = (PathBuf, String)>) -> Self {
        let roots = roots.into_iter().collect::<Vec<_>>();
        let entries = scan_skill_roots(&roots);
        Self {
            roots,
            entries: RwLock::new(entries),
        }
    }

    fn reload(&self) {
        let entries = scan_skill_roots(&self.roots);
        *self.entries.write().unwrap() = entries;
    }
}

impl SkillStore for FileSkillStore {
    async fn get(&self, name: &str) -> Result<Option<SkillDocument>, AgentError> {
        self.reload();
        let entry = {
            let entries = self.entries.read().unwrap();
            entries.get(&canonical_name(name)).cloned()
        };
        let Some(entry) = entry else {
            return Ok(None);
        };
        let content = tokio::fs::read_to_string(&entry.path)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let parsed = parse_skill_markdown(
            entry
                .path
                .parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                .unwrap_or(&entry.summary.name),
            &content,
        )?;
        Ok(Some(SkillDocument {
            name: parsed.name,
            description: parsed.description,
            content,
            source: entry.summary.source,
            root: Some(entry.root),
        }))
    }

    async fn search(&self, query: &str) -> Result<Vec<SkillSummary>, AgentError> {
        self.reload();
        let summaries = self
            .entries
            .read()
            .unwrap()
            .values()
            .map(|entry| entry.summary.clone())
            .collect();
        Ok(rank_summaries(summaries, &parse_keywords(query)))
    }

    async fn read_resource(&self, skill: &str, path: &str) -> Result<SkillResource, AgentError> {
        self.reload();
        let entry = {
            let entries = self.entries.read().unwrap();
            entries.get(&canonical_name(skill)).cloned()
        }
        .ok_or_else(|| AgentError::tool("skill__read_resource", "skill not found"))?;
        let resource_path = resolve_resource_path(&entry.root, path)?;
        let metadata = tokio::fs::symlink_metadata(&resource_path)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        if !metadata.is_file() {
            return Err(AgentError::tool(
                "skill__read_resource",
                "resource path is not a file",
            ));
        }
        let bytes = tokio::fs::read(&resource_path)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let size = bytes.len() as u64;
        match String::from_utf8(bytes) {
            Ok(content) => Ok(SkillResource {
                skill: entry.summary.name,
                path: path.to_string(),
                content: Some(content),
                size,
                binary: false,
            }),
            Err(_) => Ok(SkillResource {
                skill: entry.summary.name,
                path: path.to_string(),
                content: None,
                size,
                binary: true,
            }),
        }
    }

    fn featured_summaries(&self) -> Vec<SkillSummary> {
        self.reload();
        self.entries
            .read()
            .unwrap()
            .values()
            .map(|entry| entry.summary.clone())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSkill {
    name: String,
    description: String,
}

#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default)]
    metadata: Option<BTreeMap<String, String>>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

fn scan_skill_roots(roots: &[(PathBuf, String)]) -> BTreeMap<String, SkillEntry> {
    let mut entries = BTreeMap::new();
    for (root, source) in roots {
        let Ok(read_dir) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let skill_dir = entry.path();
            let skill_file = skill_dir.join(SKILL_FILE_NAME);
            if !skill_file.is_file() {
                continue;
            }
            let dir_name = skill_dir
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string();
            match std::fs::read_to_string(&skill_file)
                .map_err(|e| AgentError::Io(e.to_string()))
                .and_then(|content| parse_skill_markdown(&dir_name, &content))
            {
                Ok(parsed) => {
                    let key = canonical_name(&parsed.name);
                    entries.entry(key).or_insert_with(|| SkillEntry {
                        summary: SkillSummary {
                            name: parsed.name,
                            description: parsed.description,
                            source: source.clone(),
                        },
                        path: skill_file,
                        root: skill_dir,
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        path = %skill_file.display(),
                        error = %error,
                        "skipping invalid skill"
                    );
                }
            }
        }
    }
    entries
}

fn parse_skill_markdown(dir_name: &str, content: &str) -> Result<ParsedSkill, AgentError> {
    let (frontmatter, body) = split_frontmatter(content).ok_or_else(|| {
        AgentError::other("skill SKILL.md must start with YAML frontmatter delimited by ---")
    })?;
    let meta: SkillFrontmatter = serde_yaml::from_str(frontmatter)
        .map_err(|e| AgentError::other(format!("invalid skill frontmatter: {e}")))?;
    let name = meta
        .name
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::other("skill frontmatter requires non-empty name"))?;
    validate_skill_name(&name)?;
    let description = meta
        .description
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::other("skill frontmatter requires non-empty description"))?;
    if description.chars().count() > 1024 {
        return Err(AgentError::other(
            "skill frontmatter description must be at most 1024 characters",
        ));
    }
    if let Some(compatibility) = meta.compatibility.as_deref() {
        let compatibility = compatibility.trim();
        if compatibility.is_empty() || compatibility.chars().count() > 500 {
            return Err(AgentError::other(
                "skill frontmatter compatibility must be 1-500 characters when provided",
            ));
        }
    }
    let _ = (&meta.license, &meta.metadata, &meta.allowed_tools);
    if body.trim().is_empty() {
        return Err(AgentError::other("skill body must not be empty"));
    }
    if !dir_name.is_empty() && dir_name != name {
        return Err(AgentError::other(format!(
            "skill directory '{dir_name}' must match frontmatter name '{name}'"
        )));
    }
    Ok(ParsedSkill { name, description })
}

fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---")?.trim_start_matches('\n');
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    let body = &rest[end + "\n---".len()..];
    Some((frontmatter, body.trim_start_matches('\n')))
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
    keywords
}

fn rank_summaries(mut summaries: Vec<SkillSummary>, keywords: &[String]) -> Vec<SkillSummary> {
    if keywords.is_empty() {
        summaries.sort_by(|a, b| a.name.cmp(&b.name));
        return summaries;
    }

    let mut matches = summaries
        .drain(..)
        .filter_map(|summary| {
            let name = summary.name.to_ascii_lowercase();
            let description = summary.description.to_ascii_lowercase();
            let hit_count = keywords
                .iter()
                .filter(|keyword| {
                    name.contains(keyword.as_str()) || description.contains(keyword.as_str())
                })
                .count();
            (hit_count > 0).then_some((hit_count, summary))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(hits_a, a), (hits_b, b)| {
        hits_b
            .cmp(hits_a)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.source.cmp(&b.source))
    });
    matches.into_iter().map(|(_, summary)| summary).collect()
}

pub(crate) fn canonical_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn validate_skill_name(name: &str) -> Result<(), AgentError> {
    let len = name.chars().count();
    if len == 0 || len > 64 {
        return Err(AgentError::other(
            "skill frontmatter name must be 1-64 characters",
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(AgentError::other(
            "skill frontmatter name must not start or end with '-'",
        ));
    }
    if name.contains("--") {
        return Err(AgentError::other(
            "skill frontmatter name must not contain consecutive hyphens",
        ));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(AgentError::other(
            "skill frontmatter name may only contain lowercase letters, numbers, and hyphens",
        ));
    }
    Ok(())
}

fn resolve_resource_path(root: &Path, relative: &str) -> Result<PathBuf, AgentError> {
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(AgentError::tool(
            "skill__read_resource",
            "resource path must be relative",
        ));
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(AgentError::tool(
                "skill__read_resource",
                "resource path must stay inside the skill directory",
            ));
        }
    }
    Ok(root.join(path))
}

#[cfg(test)]
mod tests {
    use super::{BuiltinSkill, BuiltinSkillStore, FileSkillStore, SkillStore};
    use uuid::Uuid;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("remi-skill-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(root: &std::path::Path, slug: &str, name: &str, description: &str, body: &str) {
        let dir = root.join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}"),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn loads_standard_skill_directory() {
        let primary = temp_dir();
        let compat = temp_dir();
        write_skill(
            &primary,
            "rust-check",
            "rust-check",
            "Verify the Rust workspace",
            "Run cargo test.",
        );

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);
        let results = store.search("rust").await.unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "rust-check");
        assert_eq!(results[0].description, "Verify the Rust workspace");
        assert_eq!(results[0].source, ".remi-cat/skills");

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn loads_compat_agents_skills_directory() {
        let primary = temp_dir();
        let compat = temp_dir();
        write_skill(
            &compat,
            "researcher",
            "researcher",
            "Research workflow",
            "Use primary sources.",
        );

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);
        let results = store.search("research").await.unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, ".agents/skills");

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn primary_directory_wins_on_duplicate_skill_name() {
        let primary = temp_dir();
        let compat = temp_dir();
        write_skill(&primary, "dup", "dup", "Primary version", "Primary body.");
        write_skill(&compat, "dup", "dup", "Compat version", "Compat body.");

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);
        let doc = store.get("dup").await.unwrap().unwrap();

        assert_eq!(doc.description, "Primary version");
        assert_eq!(doc.source, ".remi-cat/skills");

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn flat_markdown_files_are_ignored() {
        let primary = temp_dir();
        let compat = temp_dir();
        std::fs::write(
            primary.join("legacy.md"),
            "---\nname: legacy\ndescription: Old\n---\n\nOld body",
        )
        .unwrap();

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);

        assert!(store.search("legacy").await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn invalid_frontmatter_is_not_loaded() {
        let primary = temp_dir();
        let compat = temp_dir();
        let dir = primary.join("missing-description");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: missing-description\n---\n\nBody",
        )
        .unwrap();

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);

        assert!(store.search("missing").await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn invalid_spec_names_are_not_loaded() {
        for (dir_name, name) in [
            ("BadName", "BadName"),
            ("bad_name", "bad_name"),
            ("bad--name", "bad--name"),
            ("-bad", "-bad"),
            ("bad-", "bad-"),
        ] {
            let primary = temp_dir();
            let compat = temp_dir();
            let dir = primary.join(dir_name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: Bad name\n---\n\nBody"),
            )
            .unwrap();

            let store = FileSkillStore::with_roots([
                (primary.clone(), ".remi-cat/skills".to_string()),
                (compat.clone(), ".agents/skills".to_string()),
            ]);
            assert!(store.search("bad").await.unwrap().is_empty());

            let _ = std::fs::remove_dir_all(primary);
            let _ = std::fs::remove_dir_all(compat);
        }
    }

    #[tokio::test]
    async fn optional_spec_frontmatter_fields_are_accepted_and_validated() {
        let primary = temp_dir();
        let compat = temp_dir();
        let dir = primary.join("with-metadata");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: with-metadata\ndescription: Has optional fields\nlicense: MIT\ncompatibility: remi-cat\nallowed-tools: bash, fs_read\nmetadata:\n  owner: remi\n---\n\nBody",
        )
        .unwrap();

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);
        assert_eq!(
            store.search("optional").await.unwrap()[0].name,
            "with-metadata"
        );

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn overlong_description_is_not_loaded() {
        let primary = temp_dir();
        let compat = temp_dir();
        let dir = primary.join("long-description");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: long-description\ndescription: {}\n---\n\nBody",
                "x".repeat(1025)
            ),
        )
        .unwrap();

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);
        assert!(store.search("long").await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn resource_read_blocks_path_traversal() {
        let primary = temp_dir();
        let compat = temp_dir();
        write_skill(&primary, "docs", "docs", "Docs", "Read resources.");

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);
        let err = store
            .read_resource("docs", "../secret.txt")
            .await
            .expect_err("path traversal should fail");
        assert!(err.to_string().contains("inside the skill directory"));

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn builtin_skills_share_registry_interface() {
        let inner = FileSkillStore::with_roots([]);
        let store = BuiltinSkillStore::new(
            inner,
            [BuiltinSkill {
                name: "trigger",
                description: "Builtin trigger capability reference",
                content: "---\nname: trigger\ndescription: Builtin trigger capability reference\n---\n\nTrigger body",
            }],
        );

        let results = store.search("trigger").await.unwrap();
        assert_eq!(results[0].name, "trigger");
        assert_eq!(
            store.get("trigger").await.unwrap().unwrap().source,
            "builtin"
        );
    }
}
