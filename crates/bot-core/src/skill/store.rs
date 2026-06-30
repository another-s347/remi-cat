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
    #[serde(default)]
    pub pin: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillLoadDiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillLoadDiagnostic {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    pub path: String,
    pub severity: SkillLoadDiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDocument {
    pub name: String,
    pub description: String,
    pub pin: bool,
    pub content: String,
    pub source: String,
    pub root: Option<PathBuf>,
    pub skill_file_path: Option<String>,
    pub resource_root_path: Option<String>,
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
    pub content: String,
}

impl BuiltinSkill {
    fn document(&self) -> Result<SkillDocument, AgentError> {
        let parsed = parse_skill_markdown(self.name, &self.content)?;
        Ok(SkillDocument {
            name: parsed.name,
            description: parsed.description,
            pin: parsed.pin,
            content: self.content.clone(),
            source: "builtin".to_string(),
            root: None,
            skill_file_path: None,
            resource_root_path: None,
        })
    }
}

#[allow(async_fn_in_trait)]
pub trait SkillStore: Send + Sync + 'static {
    async fn get(&self, name: &str) -> Result<Option<SkillDocument>, AgentError>;
    async fn search(&self, query: &str) -> Result<Vec<SkillSummary>, AgentError>;
    async fn read_resource(&self, skill: &str, path: &str) -> Result<SkillResource, AgentError>;
    fn featured_summaries(&self) -> Vec<SkillSummary>;
    fn load_diagnostics(&self) -> Vec<SkillLoadDiagnostic>;
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
                pin: false,
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
                "skill_resource",
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

    fn load_diagnostics(&self) -> Vec<SkillLoadDiagnostic> {
        self.inner.load_diagnostics()
    }
}

#[derive(Debug, Clone)]
struct SkillEntry {
    summary: SkillSummary,
    path: PathBuf,
    root: PathBuf,
    skill_file_path: Option<String>,
    resource_root_path: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SkillScan {
    entries: BTreeMap<String, SkillEntry>,
    diagnostics: Vec<SkillLoadDiagnostic>,
}

pub struct FileSkillStore {
    roots: Vec<SkillRoot>,
    entries: RwLock<BTreeMap<String, SkillEntry>>,
    diagnostics: RwLock<Vec<SkillLoadDiagnostic>>,
}

#[derive(Debug, Clone)]
struct SkillRoot {
    dir: PathBuf,
    source: String,
    workspace_root: Option<PathBuf>,
}

impl FileSkillStore {
    pub fn new(primary_dir: impl Into<PathBuf>) -> Self {
        let primary_dir = primary_dir.into();
        let workspace_root = infer_workspace_root_from_primary_dir(&primary_dir);
        Self::new_in_workspace(primary_dir, workspace_root)
    }

    pub fn new_in_workspace(
        primary_dir: impl Into<PathBuf>,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        let primary_dir = primary_dir.into();
        let workspace_root = workspace_root.into();
        let workspace_root_for_paths = Some(workspace_root.clone());
        Self::with_skill_roots([
            SkillRoot {
                dir: primary_dir,
                source: PRIMARY_SOURCE.to_string(),
                workspace_root: workspace_root_for_paths.clone(),
            },
            SkillRoot {
                dir: workspace_root.join(COMPAT_SOURCE),
                source: COMPAT_SOURCE.to_string(),
                workspace_root: workspace_root_for_paths,
            },
        ])
    }

    pub fn with_roots(roots: impl IntoIterator<Item = (PathBuf, String)>) -> Self {
        let roots = roots
            .into_iter()
            .map(|(dir, source)| SkillRoot {
                dir,
                source,
                workspace_root: None,
            })
            .collect::<Vec<_>>();
        Self::with_skill_roots(roots)
    }

    fn with_skill_roots(roots: impl IntoIterator<Item = SkillRoot>) -> Self {
        let roots = roots.into_iter().collect::<Vec<_>>();
        let scan = scan_skill_roots(&roots);
        Self {
            roots,
            entries: RwLock::new(scan.entries),
            diagnostics: RwLock::new(scan.diagnostics),
        }
    }

    fn reload(&self) {
        let scan = scan_skill_roots(&self.roots);
        *self.entries.write().unwrap() = scan.entries;
        *self.diagnostics.write().unwrap() = scan.diagnostics;
    }
}

fn infer_workspace_root_from_primary_dir(primary_dir: &Path) -> PathBuf {
    let is_skills_dir = primary_dir.file_name().and_then(|value| value.to_str()) == Some("skills");
    if !is_skills_dir {
        return PathBuf::from(".");
    }
    let Some(data_dir) = primary_dir.parent() else {
        return PathBuf::from(".");
    };
    if data_dir.file_name().and_then(|value| value.to_str()) == Some(".remi-cat") {
        let Some(workspace_root) = data_dir.parent() else {
            return PathBuf::from(".");
        };
        if workspace_root.as_os_str().is_empty() {
            return PathBuf::from(".");
        }
        return workspace_root.to_path_buf();
    }
    PathBuf::from(".")
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
            pin: parsed.pin,
            content,
            source: entry.summary.source,
            root: Some(entry.root),
            skill_file_path: entry.skill_file_path,
            resource_root_path: entry.resource_root_path,
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
        .ok_or_else(|| AgentError::tool("skill_resource", "skill not found"))?;
        let resource_path = resolve_resource_path(&entry.root, path)?;
        let metadata = tokio::fs::symlink_metadata(&resource_path)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        if !metadata.is_file() {
            return Err(AgentError::tool(
                "skill_resource",
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

    fn load_diagnostics(&self) -> Vec<SkillLoadDiagnostic> {
        self.reload();
        self.diagnostics.read().unwrap().clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSkill {
    name: String,
    description: String,
    pin: bool,
    warnings: Vec<String>,
}

fn scan_skill_roots(roots: &[SkillRoot]) -> SkillScan {
    let mut scan = SkillScan::default();
    for root_spec in roots {
        let root = &root_spec.dir;
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
                    let skill_file_path =
                        root_spec
                            .workspace_root
                            .as_ref()
                            .and_then(|workspace_root| {
                                sandbox_relative_path(workspace_root, &skill_file)
                            });
                    let resource_root_path =
                        root_spec
                            .workspace_root
                            .as_ref()
                            .and_then(|workspace_root| {
                                sandbox_relative_path(workspace_root, &skill_dir)
                            });
                    if root_spec.workspace_root.is_some() && skill_file_path.is_none() {
                        scan.diagnostics.push(SkillLoadDiagnostic {
                            skill: Some(parsed.name.clone()),
                            path: skill_file.display().to_string(),
                            severity: SkillLoadDiagnosticSeverity::Warning,
                            message: "skill is outside the workspace root; resources cannot be read via fs_read".to_string(),
                        });
                    }
                    for warning in parsed.warnings {
                        scan.diagnostics.push(SkillLoadDiagnostic {
                            skill: Some(parsed.name.clone()),
                            path: skill_file.display().to_string(),
                            severity: SkillLoadDiagnosticSeverity::Warning,
                            message: warning,
                        });
                    }
                    scan.entries.entry(key).or_insert_with(|| SkillEntry {
                        summary: SkillSummary {
                            name: parsed.name,
                            description: parsed.description,
                            source: root_spec.source.clone(),
                            pin: parsed.pin,
                        },
                        path: skill_file,
                        root: skill_dir,
                        skill_file_path,
                        resource_root_path,
                    });
                }
                Err(error) => {
                    scan.diagnostics.push(SkillLoadDiagnostic {
                        skill: normalize_skill_name(&dir_name),
                        path: skill_file.display().to_string(),
                        severity: SkillLoadDiagnosticSeverity::Error,
                        message: error.to_string(),
                    });
                    tracing::warn!(
                        path = %skill_file.display(),
                        error = %error,
                        "skipping invalid skill"
                    );
                }
            }
        }
    }
    scan
}

fn parse_skill_markdown(dir_name: &str, content: &str) -> Result<ParsedSkill, AgentError> {
    let mut warnings = Vec::new();
    let (frontmatter, body) = match split_frontmatter(content) {
        Some(parts) => parts,
        None => {
            warnings.push(
                "missing YAML frontmatter; inferred skill metadata from file content".to_string(),
            );
            ("", content)
        }
    };
    let meta = if frontmatter.trim().is_empty() {
        serde_yaml::Value::Mapping(Default::default())
    } else {
        serde_yaml::from_str::<serde_yaml::Value>(frontmatter)
            .map_err(|e| AgentError::other(format!("invalid skill frontmatter: {e}")))?
    };
    let mapping = meta.as_mapping();

    let raw_name = mapping
        .and_then(|map| yaml_field(map, "name"))
        .and_then(yaml_scalarish_string)
        .filter(|value| !value.trim().is_empty());
    let name_source = raw_name.as_deref().unwrap_or(dir_name);
    let name = normalize_skill_name(name_source)
        .or_else(|| normalize_skill_name(dir_name))
        .ok_or_else(|| AgentError::other("skill requires a usable name or directory name"))?;
    if raw_name.is_none() {
        warnings.push(format!(
            "missing or non-string frontmatter name; using `{name}` from directory name"
        ));
    } else if raw_name.as_deref() != Some(name.as_str()) {
        warnings.push(format!(
            "normalized frontmatter name `{}` to `{name}`",
            raw_name.unwrap_or_default()
        ));
    }

    let mut description = mapping
        .and_then(|map| yaml_field(map, "description"))
        .and_then(yaml_scalarish_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| derive_description(body, dir_name));
    if description.is_none() {
        description = Some(format!("Local skill from {dir_name}"));
        warnings.push("missing description; using a generated fallback".to_string());
    } else if mapping
        .and_then(|map| yaml_field(map, "description"))
        .is_none()
    {
        warnings.push("missing description; inferred from skill body".to_string());
    }
    let mut description = description.unwrap_or_else(|| format!("Local skill from {dir_name}"));
    if description.chars().count() > 1024 {
        description = truncate_chars(&description, 1024);
        warnings.push("description exceeded 1024 characters; truncated".to_string());
    }

    let pin = mapping
        .and_then(|map| yaml_field(map, "pin"))
        .map(|value| match value {
            serde_yaml::Value::Bool(value) => *value,
            _ => {
                warnings.push("frontmatter field `pin` must be boolean; using false".to_string());
                false
            }
        })
        .unwrap_or(false);

    if let Some(map) = mapping {
        for field in ["compatibility", "metadata", "allowed-tools", "license"] {
            if let Some(value) = yaml_field(map, field) {
                if !matches!(
                    value,
                    serde_yaml::Value::Null
                        | serde_yaml::Value::Bool(_)
                        | serde_yaml::Value::Number(_)
                        | serde_yaml::Value::String(_)
                ) {
                    warnings.push(format!(
                        "frontmatter field `{field}` uses a non-scalar value; accepted leniently"
                    ));
                }
            }
        }
    }

    Ok(ParsedSkill {
        name,
        description,
        pin,
        warnings,
    })
}

fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let content = trim_leading_skill_preamble(content);
    let rest = content.strip_prefix("---")?.trim_start_matches('\n');
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    let body = &rest[end + "\n---".len()..];
    Some((frontmatter, body.trim_start_matches('\n')))
}

fn trim_leading_skill_preamble(mut content: &str) -> &str {
    loop {
        content = content.trim_start_matches('\u{feff}');
        content = content.trim_start();
        let Some(comment_start) = content
            .strip_prefix("<!--")
            .or_else(|| content.strip_prefix("<--"))
        else {
            return content;
        };
        let Some(end) = comment_start.find("-->") else {
            return content;
        };
        content = &comment_start[end + "-->".len()..];
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
    normalize_skill_name(name).unwrap_or_else(|| name.trim().to_ascii_lowercase())
}

fn yaml_field<'a>(map: &'a serde_yaml::Mapping, name: &str) -> Option<&'a serde_yaml::Value> {
    map.get(serde_yaml::Value::String(name.to_string()))
}

fn yaml_scalarish_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(value) => Some(value.clone()),
        serde_yaml::Value::Number(value) => Some(value.to_string()),
        serde_yaml::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn derive_description(body: &str, dir_name: &str) -> Option<String> {
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let candidate = line.trim_start_matches('#').trim();
        if !candidate.is_empty() {
            return Some(truncate_chars(candidate, 1024));
        }
    }
    normalize_skill_name(dir_name).map(|name| format!("Local skill from {name}"))
}

fn normalize_skill_name(raw: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut last_was_dash = false;
    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            normalized.push('-');
            last_was_dash = true;
        }
    }
    let normalized = normalized.trim_matches('-').to_string();
    if normalized.is_empty() {
        None
    } else {
        Some(
            truncate_chars(&normalized, 64)
                .trim_matches('-')
                .to_string(),
        )
        .filter(|value| !value.is_empty())
    }
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn sandbox_relative_path(workspace_root: &Path, path: &Path) -> Option<String> {
    let workspace_root = absolute_lexical_path(workspace_root);
    let path = absolute_lexical_path(path);
    let relative = path.strip_prefix(&workspace_root).ok()?;
    if relative.as_os_str().is_empty() {
        return Some(".".to_string());
    }
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn absolute_lexical_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn resolve_resource_path(root: &Path, relative: &str) -> Result<PathBuf, AgentError> {
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(AgentError::tool(
            "skill_resource",
            "resource path must be relative",
        ));
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(AgentError::tool(
                "skill_resource",
                "resource path must stay inside the skill directory",
            ));
        }
    }
    Ok(root.join(path))
}

#[cfg(test)]
mod tests {
    use super::{
        BuiltinSkill, BuiltinSkillStore, FileSkillStore, SkillLoadDiagnosticSeverity, SkillStore,
    };
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
    async fn new_in_workspace_loads_primary_and_compat_directories_from_same_workspace() {
        let workspace = temp_dir();
        let primary = workspace.join(".remi-cat").join("skills");
        let compat = workspace.join(".agents").join("skills");
        write_skill(
            &primary,
            "primary-skill",
            "primary-skill",
            "Primary workflow",
            "Use the primary skill.",
        );
        write_skill(
            &compat,
            "compat-skill",
            "compat-skill",
            "Compat workflow",
            "Use the compat skill.",
        );

        let store = FileSkillStore::new_in_workspace(&primary, &workspace);
        let primary_doc = store.get("primary-skill").await.unwrap().unwrap();
        let compat_doc = store.get("compat-skill").await.unwrap().unwrap();

        assert_eq!(primary_doc.source, ".remi-cat/skills");
        assert_eq!(compat_doc.source, ".agents/skills");
        assert_eq!(
            primary_doc.skill_file_path.as_deref(),
            Some(".remi-cat/skills/primary-skill/SKILL.md")
        );
        assert_eq!(
            primary_doc.resource_root_path.as_deref(),
            Some(".remi-cat/skills/primary-skill")
        );
        assert_eq!(
            compat_doc.skill_file_path.as_deref(),
            Some(".agents/skills/compat-skill/SKILL.md")
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn new_in_workspace_prefers_primary_directory_duplicates() {
        let workspace = temp_dir();
        let primary = workspace.join(".remi-cat").join("skills");
        let compat = workspace.join(".agents").join("skills");
        write_skill(&primary, "dup", "dup", "Primary version", "Primary body.");
        write_skill(&compat, "dup", "dup", "Compat version", "Compat body.");

        let store = FileSkillStore::new_in_workspace(&primary, &workspace);
        let doc = store.get("dup").await.unwrap().unwrap();

        assert_eq!(doc.description, "Primary version");
        assert_eq!(doc.source, ".remi-cat/skills");

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn skill_name_does_not_need_to_match_directory_name() {
        let primary = temp_dir();
        let compat = temp_dir();
        write_skill(
            &primary,
            "directory-slug",
            "frontmatter-name",
            "Named by frontmatter",
            "Body.",
        );

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);

        assert!(store.get("directory-slug").await.unwrap().is_none());
        let doc = store.get("frontmatter-name").await.unwrap().unwrap();
        assert_eq!(doc.name, "frontmatter-name");
        assert_eq!(doc.description, "Named by frontmatter");

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
    async fn missing_description_is_inferred_from_body() {
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

        let doc = store.get("missing-description").await.unwrap().unwrap();
        assert_eq!(doc.description, "Body");
        assert!(store
            .load_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("missing description")));

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn pin_frontmatter_is_optional_and_boolean() {
        let primary = temp_dir();
        let compat = temp_dir();
        let pinned = primary.join("pinned");
        std::fs::create_dir_all(&pinned).unwrap();
        std::fs::write(
            pinned.join("SKILL.md"),
            "---\nname: pinned\ndescription: Pinned workflow\npin: true\n---\n\nBody",
        )
        .unwrap();
        let bad_pin = primary.join("bad-pin");
        std::fs::create_dir_all(&bad_pin).unwrap();
        std::fs::write(
            bad_pin.join("SKILL.md"),
            "---\nname: bad-pin\ndescription: Bad pin\npin: \"true\"\n---\n\nBody",
        )
        .unwrap();
        write_skill(
            &primary,
            "default-pin",
            "default-pin",
            "Default pin",
            "Body",
        );

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);

        assert!(store.get("pinned").await.unwrap().unwrap().pin);
        assert!(store
            .featured_summaries()
            .into_iter()
            .any(|summary| summary.name == "pinned" && summary.pin));
        assert!(!store.get("default-pin").await.unwrap().unwrap().pin);
        assert!(!store.get("bad-pin").await.unwrap().unwrap().pin);
        assert!(store
            .load_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("field `pin` must be boolean")));

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn leading_html_comments_do_not_hide_frontmatter() {
        for prefix in [
            "<!-- generated by installer -->\n",
            "\n<-- legacy generated comment -->\n\n",
        ] {
            let primary = temp_dir();
            let compat = temp_dir();
            let dir = primary.join("commented");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("{prefix}---\nname: commented\ndescription: Commented skill\n---\n\nBody"),
            )
            .unwrap();

            let store = FileSkillStore::with_roots([
                (primary.clone(), ".remi-cat/skills".to_string()),
                (compat.clone(), ".agents/skills".to_string()),
            ]);
            let doc = store.get("commented").await.unwrap().unwrap();

            assert_eq!(doc.name, "commented");
            assert_eq!(doc.description, "Commented skill");
            assert!(!store
                .load_diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("missing YAML frontmatter")));

            let _ = std::fs::remove_dir_all(primary);
            let _ = std::fs::remove_dir_all(compat);
        }
    }

    #[tokio::test]
    async fn non_spec_names_are_normalized_and_loaded() {
        for (dir_name, name, expected) in [
            ("BadName", "BadName", "badname"),
            ("bad_name", "bad_name", "bad-name"),
            ("bad--name", "bad--name", "bad-name"),
            ("-bad", "-bad", "bad"),
            ("bad-", "bad-", "bad"),
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
            let doc = store.get(name).await.unwrap().unwrap();
            assert_eq!(doc.name, expected);
            assert!(store
                .load_diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.message.contains("normalized frontmatter name")));

            let _ = std::fs::remove_dir_all(primary);
            let _ = std::fs::remove_dir_all(compat);
        }
    }

    #[tokio::test]
    async fn optional_spec_frontmatter_fields_accept_non_scalar_values() {
        let primary = temp_dir();
        let compat = temp_dir();
        let dir = primary.join("with-metadata");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: with-metadata\ndescription: Has optional fields\nlicense: MIT\ncompatibility:\n  - remi-cat\nallowed-tools:\n  - bash\n  - fs_read\nmetadata:\n  owner:\n    name: remi\n---\n\nBody",
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
        assert!(store
            .load_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("non-scalar value")));

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn overlong_description_is_truncated_with_warning() {
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
        let doc = store.get("long-description").await.unwrap().unwrap();
        assert_eq!(doc.description.chars().count(), 1024);
        assert!(store
            .load_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("truncated")));

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn markdown_without_frontmatter_loads_with_inferred_metadata() {
        let primary = temp_dir();
        let compat = temp_dir();
        let dir = primary.join("Loose Skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# Loose workflow\n\nUse it.").unwrap();

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);
        let doc = store.get("loose_skill").await.unwrap().unwrap();

        assert_eq!(doc.name, "loose-skill");
        assert_eq!(doc.description, "Loose workflow");
        assert!(store.load_diagnostics().iter().any(|diagnostic| {
            diagnostic.severity == SkillLoadDiagnosticSeverity::Warning
                && diagnostic.message.contains("missing YAML frontmatter")
        }));

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }

    #[tokio::test]
    async fn invalid_yaml_frontmatter_is_reported_as_error() {
        let primary = temp_dir();
        let compat = temp_dir();
        let dir = primary.join("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: [broken\ndescription: Bad\n---\n\nBody",
        )
        .unwrap();

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);

        assert!(store.search("broken").await.unwrap().is_empty());
        assert!(store.load_diagnostics().iter().any(|diagnostic| {
            diagnostic.severity == SkillLoadDiagnosticSeverity::Error
                && diagnostic.message.contains("invalid skill frontmatter")
        }));

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
                content: "---\nname: trigger\ndescription: Builtin trigger capability reference\n---\n\nTrigger body".to_string(),
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
