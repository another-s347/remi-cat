use remi_agentloop::prelude::AgentError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use crate::search_query::tokenize_search_query;

const SKILL_FILE_NAME: &str = "SKILL.md";
const PRIMARY_SOURCE: &str = ".remi-cat/skills";
const COMPAT_SOURCE: &str = ".agents/skills";
const MAX_SKILL_DEPTH: usize = 6;
const MAX_SCANNED_DIRS: usize = 2000;
const MAX_SKILL_FILE_SIZE: u64 = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub parent_id: Option<String>,
    pub has_children: bool,
    pub source: String,
    pub skill_file_path: Option<String>,
    pub resource_root_path: Option<String>,
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
    pub id: String,
    pub name: String,
    pub description: String,
    pub pin: bool,
    pub content: String,
    pub source: String,
    pub root: Option<PathBuf>,
    pub skill_file_path: Option<String>,
    pub resource_root_path: Option<String>,
    pub parent_id: Option<String>,
    pub children: Vec<SkillSummary>,
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
            id: parsed.name.clone(),
            name: parsed.name,
            description: parsed.description,
            pin: parsed.pin,
            content: self.content.clone(),
            source: "builtin".to_string(),
            root: None,
            skill_file_path: None,
            resource_root_path: None,
            parent_id: None,
            children: Vec::new(),
        })
    }
}

#[allow(async_fn_in_trait)]
pub trait SkillStore: Send + Sync + 'static {
    async fn get(&self, name: &str) -> Result<Option<SkillDocument>, AgentError>;
    async fn search(&self, query: &str) -> Result<Vec<SkillSummary>, AgentError>;
    async fn read_resource(&self, skill: &str, path: &str) -> Result<SkillResource, AgentError>;
    fn featured_summaries(&self) -> Vec<SkillSummary>;
    fn all_summaries(&self) -> Vec<SkillSummary>;
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
                id: skill.name.to_string(),
                name: skill.name.to_string(),
                description: skill.description.to_string(),
                parent_id: None,
                has_children: false,
                source: "builtin".to_string(),
                skill_file_path: None,
                resource_root_path: None,
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
            .map(|entry| canonical_name(&entry.id))
            .collect();

        for entry in self.inner.search(query).await? {
            if seen.insert(canonical_name(&entry.id)) {
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
            .map(|entry| canonical_name(&entry.id))
            .collect();

        for summary in self.inner.featured_summaries() {
            if seen.insert(canonical_name(&summary.id)) {
                featured.push(summary);
            }
        }

        featured
    }

    fn all_summaries(&self) -> Vec<SkillSummary> {
        let mut all = self.builtin_summaries();
        let mut seen: HashSet<String> = all.iter().map(|entry| canonical_id(&entry.id)).collect();
        for summary in self.inner.all_summaries() {
            if seen.insert(canonical_id(&summary.id)) {
                all.push(summary);
            }
        }
        all
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
    metadata_cache: RwLock<BTreeMap<PathBuf, CachedSkillMetadata>>,
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
        let mut metadata_cache = BTreeMap::new();
        let scan = scan_skill_roots(&roots, &mut metadata_cache);
        Self {
            roots,
            entries: RwLock::new(scan.entries),
            diagnostics: RwLock::new(scan.diagnostics),
            metadata_cache: RwLock::new(metadata_cache),
        }
    }

    fn reload(&self) {
        let mut metadata_cache = self.metadata_cache.write().unwrap();
        let scan = scan_skill_roots(&self.roots, &mut metadata_cache);
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
            resolve_entry(&entries, name)?
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
            id: entry.summary.id.clone(),
            name: parsed.name,
            description: parsed.description,
            pin: parsed.pin,
            content,
            source: entry.summary.source,
            root: Some(entry.root),
            skill_file_path: entry.skill_file_path,
            resource_root_path: entry.resource_root_path,
            parent_id: entry.summary.parent_id.clone(),
            children: self
                .entries
                .read()
                .unwrap()
                .values()
                .filter(|child| child.summary.parent_id.as_deref() == Some(&entry.summary.id))
                .map(|child| child.summary.clone())
                .collect(),
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
            resolve_entry(&entries, skill)?
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
                skill: entry.summary.id.clone(),
                path: path.to_string(),
                content: Some(content),
                size,
                binary: false,
            }),
            Err(_) => Ok(SkillResource {
                skill: entry.summary.id.clone(),
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
            .filter(|entry| entry.summary.parent_id.is_none() || entry.summary.pin)
            .map(|entry| entry.summary.clone())
            .collect()
    }

    fn all_summaries(&self) -> Vec<SkillSummary> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillFileSignature {
    modified: Option<SystemTime>,
    size: u64,
}

#[derive(Debug, Clone)]
struct CachedSkillMetadata {
    signature: SkillFileSignature,
    parsed: ParsedSkill,
}

fn scan_skill_roots(
    roots: &[SkillRoot],
    metadata_cache: &mut BTreeMap<PathBuf, CachedSkillMetadata>,
) -> SkillScan {
    let mut scan = SkillScan::default();
    let mut seen_files = HashSet::new();
    for root_spec in roots {
        let mut dirs_seen = 0;
        scan_container(
            root_spec,
            &root_spec.dir,
            None,
            0,
            &mut dirs_seen,
            &mut scan,
            metadata_cache,
            &mut seen_files,
        );
    }
    metadata_cache.retain(|path, _| seen_files.contains(path));
    let parent_ids: HashSet<String> = scan
        .entries
        .values()
        .filter_map(|entry| entry.summary.parent_id.clone())
        .collect();
    for entry in scan.entries.values_mut() {
        entry.summary.has_children = parent_ids.contains(&entry.summary.id);
    }
    scan
}

fn scan_container(
    root_spec: &SkillRoot,
    container: &Path,
    parent_id: Option<&str>,
    depth: usize,
    dirs_seen: &mut usize,
    scan: &mut SkillScan,
    metadata_cache: &mut BTreeMap<PathBuf, CachedSkillMetadata>,
    seen_files: &mut HashSet<PathBuf>,
) {
    let Ok(read_dir) = std::fs::read_dir(container) else {
        return;
    };
    let mut dirs = read_dir
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|ty| ty.is_dir()))
        .collect::<Vec<_>>();
    dirs.sort_by_key(|entry| entry.path());
    for dir in dirs {
        *dirs_seen += 1;
        if *dirs_seen > MAX_SCANNED_DIRS {
            scan.diagnostics.push(SkillLoadDiagnostic {
                skill: parent_id.map(str::to_string),
                path: container.display().to_string(),
                severity: SkillLoadDiagnosticSeverity::Error,
                message: format!("skill scan exceeded {MAX_SCANNED_DIRS} directories"),
            });
            return;
        }
        let skill_dir = dir.path();
        if std::fs::symlink_metadata(&skill_dir).is_ok_and(|m| m.file_type().is_symlink()) {
            continue;
        }
        let skill_file = skill_dir.join(SKILL_FILE_NAME);
        let Ok(file_metadata) = std::fs::metadata(&skill_file) else {
            continue;
        };
        if !file_metadata.is_file() {
            continue;
        }
        seen_files.insert(skill_file.clone());
        if depth > MAX_SKILL_DEPTH {
            scan.diagnostics.push(SkillLoadDiagnostic {
                skill: parent_id.map(str::to_string),
                path: skill_file.display().to_string(),
                severity: SkillLoadDiagnosticSeverity::Error,
                message: format!("maximum child skill depth {MAX_SKILL_DEPTH} exceeded"),
            });
            continue;
        }
        let dir_name = skill_dir
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or_default()
            .to_string();
        let size = file_metadata.len();
        let signature = SkillFileSignature {
            modified: file_metadata.modified().ok(),
            size,
        };
        let parsed = if size > MAX_SKILL_FILE_SIZE {
            Err(AgentError::other(format!(
                "SKILL.md exceeds {MAX_SKILL_FILE_SIZE} byte limit"
            )))
        } else if let Some(cached) = metadata_cache
            .get(&skill_file)
            .filter(|cached| cached.signature == signature)
        {
            Ok(cached.parsed.clone())
        } else {
            std::fs::read_to_string(&skill_file)
                .map_err(|e| AgentError::Io(e.to_string()))
                .and_then(|content| parse_skill_markdown(&dir_name, &content))
                .inspect(|parsed| {
                    metadata_cache.insert(
                        skill_file.clone(),
                        CachedSkillMetadata {
                            signature: signature.clone(),
                            parsed: parsed.clone(),
                        },
                    );
                })
        };
        match parsed {
            Ok(parsed) => {
                let id = parent_id
                    .map(|p| format!("{p}/{}", parsed.name))
                    .unwrap_or_else(|| parsed.name.clone());
                let skill_file_path = root_spec
                    .workspace_root
                    .as_ref()
                    .and_then(|root| safe_workspace_relative_path(root, &skill_file));
                let resource_root_path = root_spec
                    .workspace_root
                    .as_ref()
                    .and_then(|root| safe_workspace_relative_path(root, &skill_dir));
                for warning in parsed.warnings {
                    scan.diagnostics.push(SkillLoadDiagnostic {
                        skill: Some(id.clone()),
                        path: skill_file.display().to_string(),
                        severity: SkillLoadDiagnosticSeverity::Warning,
                        message: warning,
                    });
                }
                let candidate = SkillEntry {
                    summary: SkillSummary {
                        id: id.clone(),
                        name: parsed.name,
                        description: parsed.description,
                        parent_id: parent_id.map(str::to_string),
                        has_children: false,
                        source: root_spec.source.clone(),
                        skill_file_path: skill_file_path.clone(),
                        resource_root_path: resource_root_path.clone(),
                        pin: parsed.pin,
                    },
                    path: skill_file,
                    root: skill_dir.clone(),
                    skill_file_path,
                    resource_root_path,
                };
                let key = canonical_id(&id);
                if let Some(existing) = scan.entries.get(&key) {
                    scan.diagnostics.push(SkillLoadDiagnostic {
                        skill: Some(id.clone()),
                        path: candidate.path.display().to_string(),
                        severity: SkillLoadDiagnosticSeverity::Warning,
                        message: format!(
                            "shadowed by {} from {}",
                            existing.path.display(),
                            existing.summary.source
                        ),
                    });
                } else {
                    scan.entries.insert(key, candidate);
                }
                scan_container(
                    root_spec,
                    &skill_dir.join("skills"),
                    Some(&id),
                    depth + 1,
                    dirs_seen,
                    scan,
                    metadata_cache,
                    seen_files,
                );
            }
            Err(error) => scan.diagnostics.push(SkillLoadDiagnostic {
                skill: normalize_skill_name(&dir_name),
                path: skill_file.display().to_string(),
                severity: SkillLoadDiagnosticSeverity::Error,
                message: error.to_string(),
            }),
        }
    }
}

fn resolve_entry(
    entries: &BTreeMap<String, SkillEntry>,
    locator: &str,
) -> Result<Option<SkillEntry>, AgentError> {
    if locator.contains('/') {
        return Ok(entries.get(&canonical_id(locator)).cloned());
    }
    let needle = canonical_name(locator);
    let matches = entries
        .values()
        .filter(|entry| canonical_name(&entry.summary.name) == needle)
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [entry] => Ok(Some(entry.clone())),
        many => Err(AgentError::tool(
            "skill",
            format!(
                "skill name '{locator}' is ambiguous; use one of: {}",
                many.iter()
                    .map(|e| e.summary.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

fn canonical_id(id: &str) -> String {
    id.split('/')
        .map(canonical_name)
        .collect::<Vec<_>>()
        .join("/")
}

fn parse_skill_markdown(dir_name: &str, content: &str) -> Result<ParsedSkill, AgentError> {
    let mut warnings = Vec::new();
    let (frontmatter, _body) = match split_frontmatter(content) {
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
    if canonical_name(dir_name) != canonical_name(&name) {
        warnings.push(format!(
            "frontmatter name `{name}` does not match leaf directory `{dir_name}`"
        ));
    }

    let mut description = mapping
        .and_then(|map| yaml_field(map, "description"))
        .and_then(yaml_scalarish_string)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AgentError::other("skill requires a non-empty frontmatter description"))?;
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
    tokenize_search_query(query)
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

fn safe_workspace_relative_path(workspace_root: &Path, path: &Path) -> Option<String> {
    let canonical_root = std::fs::canonicalize(workspace_root).ok()?;
    let canonical_path = std::fs::canonicalize(path).ok()?;
    if !canonical_path.starts_with(&canonical_root) {
        return None;
    }
    let mut current = canonical_path.as_path();
    while current != canonical_root {
        if std::fs::symlink_metadata(current)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
        current = current.parent()?;
    }
    sandbox_relative_path(&canonical_root, &canonical_path)
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
    async fn missing_description_is_rejected() {
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

        assert!(store.get("missing-description").await.unwrap().is_none());
        assert!(store
            .load_diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("frontmatter description")));

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
    async fn markdown_without_frontmatter_is_rejected() {
        let primary = temp_dir();
        let compat = temp_dir();
        let dir = primary.join("Loose Skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# Loose workflow\n\nUse it.").unwrap();

        let store = FileSkillStore::with_roots([
            (primary.clone(), ".remi-cat/skills".to_string()),
            (compat.clone(), ".agents/skills".to_string()),
        ]);
        assert!(store.get("loose_skill").await.unwrap().is_none());
        assert!(store.load_diagnostics().iter().any(|diagnostic| {
            diagnostic.severity == SkillLoadDiagnosticSeverity::Error
                && diagnostic.message.contains("frontmatter description")
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
                name: "docs",
                description: "Builtin docs capability reference",
                content: "---\nname: docs\ndescription: Builtin docs capability reference\n---\n\nDocs body"
                    .to_string(),
            }],
        );

        let results = store.search("docs").await.unwrap();
        assert_eq!(results[0].name, "docs");
        assert_eq!(store.get("docs").await.unwrap().unwrap().source, "builtin");
    }

    #[tokio::test]
    async fn discovers_progressive_child_skills_and_exposes_safe_paths() {
        let workspace = temp_dir();
        let primary = workspace.join(".remi-cat/skills");
        write_skill(
            &primary,
            "office",
            "office",
            "Office workflows",
            "Choose a child skill.",
        );
        let excel_root = primary.join("office/skills");
        write_skill(
            &excel_root,
            "excel",
            "excel",
            "Excel workflows",
            "Work with spreadsheets.",
        );
        let formula_root = excel_root.join("excel/skills");
        write_skill(
            &formula_root,
            "formulas",
            "formulas",
            "Formula workflows",
            "Write formulas.",
        );
        std::fs::create_dir_all(primary.join("office/references/not-a-skill")).unwrap();
        write_skill(
            &primary.join("office/references"),
            "not-a-skill",
            "hidden",
            "Hidden",
            "Never scan me.",
        );

        let store = FileSkillStore::new_in_workspace(&primary, &workspace);
        let featured = store.featured_summaries();
        assert_eq!(
            featured.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["office"]
        );
        let results = store.search("formula").await.unwrap();
        assert_eq!(results[0].id, "office/excel/formulas");
        assert!(!store
            .search("hidden")
            .await
            .unwrap()
            .iter()
            .any(|s| s.name == "hidden"));

        let office = store.get("office").await.unwrap().unwrap();
        assert_eq!(
            office
                .children
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["office/excel"]
        );
        let excel = store.get("office/excel").await.unwrap().unwrap();
        assert_eq!(excel.parent_id.as_deref(), Some("office"));
        assert_eq!(
            excel.skill_file_path.as_deref(),
            Some(".remi-cat/skills/office/skills/excel/SKILL.md")
        );
        assert_eq!(
            excel.resource_root_path.as_deref(),
            Some(".remi-cat/skills/office/skills/excel")
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn ambiguous_leaf_name_requires_hierarchical_id() {
        let root = temp_dir();
        for parent in ["office", "finance"] {
            write_skill(&root, parent, parent, parent, "Parent");
            write_skill(
                &root.join(parent).join("skills"),
                "excel",
                "excel",
                "Excel",
                "Child",
            );
        }
        let store = FileSkillStore::with_roots([(root.clone(), "test".to_string())]);
        let error = store
            .get("excel")
            .await
            .expect_err("leaf alias must be ambiguous");
        assert!(error.to_string().contains("office/excel"));
        assert_eq!(
            store.get("finance/excel").await.unwrap().unwrap().id,
            "finance/excel"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn loads_symlinked_skill_markdown() {
        use std::os::unix::fs::symlink;

        let root = temp_dir();
        let outside = temp_dir().join("outside.md");
        std::fs::write(
            &outside,
            "---\nname: linked\ndescription: Outside instructions\n---\n\nNever load this.",
        )
        .unwrap();
        let skill_dir = root.join("linked");
        std::fs::create_dir_all(&skill_dir).unwrap();
        symlink(&outside, skill_dir.join("SKILL.md")).unwrap();

        let store = FileSkillStore::with_roots([(root.clone(), "test".to_string())]);
        let document = store.get("linked").await.unwrap().unwrap();
        assert_eq!(document.id, "linked");
        assert!(document.content.contains("Never load this."));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(outside);
    }
}
