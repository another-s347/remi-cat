//! Skill store trait and implementations.
//!
//! Each skill is a named markdown document the model can create, read, list,
//! and delete.  The `FileSkillStore` persists skills to disk — one `.md` file
//! per skill — mirroring the Claude Code `.claude/commands/` convention.

use remi_core::error::AgentError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ── SkillStore trait ──────────────────────────────────────────────────────────

/// Persistent skill storage backend.
pub trait SkillStore: Send + Sync + 'static {
    /// Save (create or overwrite) a skill.  Returns the storage path/key.
    async fn save(&self, name: &str, content: &str) -> Result<String, AgentError>;
    /// Retrieve a skill's content.  Returns `None` if the skill doesn't exist.
    async fn get(&self, name: &str) -> Result<Option<String>, AgentError>;
    /// List all skill names.
    async fn list(&self) -> Result<Vec<String>, AgentError>;
    /// Delete a skill.  Returns `true` if it existed.
    async fn delete(&self, name: &str) -> Result<bool, AgentError>;
}

// ── FileSkillStore ────────────────────────────────────────────────────────────

/// Stores each skill following the Claude Code convention:
/// `<base_dir>/<skill-name>/SKILL.md`
///
/// Supporting files (examples, templates, scripts) may also live in the skill
/// sub-directory alongside `SKILL.md`, though only `SKILL.md` is managed by
/// the tool API.
///
/// **Backward compatibility**: if `<base_dir>/<name>.md` (legacy flat format)
/// exists and no subdirectory skill was found, it is read transparently.  On
/// the next `save()` the skill is automatically upgraded to the new structure.
#[derive(Clone)]
pub struct FileSkillStore {
    base_dir: PathBuf,
}

impl FileSkillStore {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Canonical path: `<base_dir>/<name>/SKILL.md`
    fn skill_md_path(&self, name: &str) -> PathBuf {
        let safe = name.replace(['/', '\\'], "_");
        self.base_dir.join(&safe).join("SKILL.md")
    }

    /// Legacy flat path: `<base_dir>/<name>.md` (read-only fallback)
    fn legacy_path(&self, name: &str) -> PathBuf {
        let safe = name.replace(['/', '\\', '.'], "_");
        self.base_dir.join(format!("{safe}.md"))
    }

    /// Synchronously list all skills with their optional `description` field
    /// parsed from YAML frontmatter.  Used during `build()` to inject skill
    /// summaries into the system prompt.
    ///
    /// Returns `Vec<(name, description_or_None)>` sorted by name.
    pub fn list_with_descriptions_sync(&self) -> Vec<(String, Option<String>)> {
        let mut result: Vec<(String, Option<String>)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // ── New-style: subdirectories with SKILL.md ───────────────────────────
        if let Ok(rd) = std::fs::read_dir(&self.base_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let skill_md = path.join("SKILL.md");
                    if skill_md.is_file() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            let content = std::fs::read_to_string(&skill_md).unwrap_or_default();
                            let desc = parse_frontmatter_description(&content);
                            seen.insert(name.to_string());
                            result.push((name.to_string(), desc));
                        }
                    }
                }
            }
        }

        // ── Legacy-style: flat <name>.md files (skip names already found) ─────
        if let Ok(rd) = std::fs::read_dir(&self.base_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if path.extension().and_then(|e| e.to_str()) == Some("md") {
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            if !seen.contains(stem) {
                                let content = std::fs::read_to_string(&path).unwrap_or_default();
                                let desc = parse_frontmatter_description(&content);
                                seen.insert(stem.to_string());
                                result.push((stem.to_string(), desc));
                            }
                        }
                    }
                }
            }
        }

        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }
}

/// Parse the `description:` value from YAML frontmatter (`---` … `---`) in a
/// skill document.  Returns `None` if no frontmatter or no `description` field.
pub fn parse_frontmatter_description(content: &str) -> Option<String> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }
    // Find closing ---
    let rest = &content[3..];
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("description:") {
            let v = val.trim().trim_matches('"').trim_matches('\'').to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

impl SkillStore for FileSkillStore {
    async fn save(&self, name: &str, content: &str) -> Result<String, AgentError> {
        let path = self.skill_md_path(name);
        // Create `<base_dir>/<name>/` directory
        let dir = path.parent().unwrap();
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        // Remove legacy flat file if it exists (auto-migrate on first save)
        let legacy = self.legacy_path(name);
        if legacy.exists() {
            let _ = tokio::fs::remove_file(&legacy).await;
        }
        Ok(path.to_string_lossy().to_string())
    }

    async fn get(&self, name: &str) -> Result<Option<String>, AgentError> {
        // Try canonical path first
        let path = self.skill_md_path(name);
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => return Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(AgentError::Io(e.to_string())),
        }
        // Fall back to legacy flat file
        let legacy = self.legacy_path(name);
        match tokio::fs::read_to_string(&legacy).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AgentError::Io(e.to_string())),
        }
    }

    async fn list(&self) -> Result<Vec<String>, AgentError> {
        let dir = &self.base_dir;
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut rd = tokio::fs::read_dir(dir)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                // New-style: directory containing SKILL.md
                if path.join("SKILL.md").exists() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        names.insert(name.to_string());
                    }
                }
            } else if path.is_file() {
                // Legacy-style: flat <name>.md
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.insert(stem.to_string());
                    }
                }
            }
        }
        let mut names: Vec<String> = names.into_iter().collect();
        names.sort();
        Ok(names)
    }

    async fn delete(&self, name: &str) -> Result<bool, AgentError> {
        let mut deleted = false;
        // Remove canonical skill directory
        let dir = self.skill_md_path(name).parent().unwrap().to_path_buf();
        if dir.exists() {
            // Remove the whole skill sub-directory
            if tokio::fs::remove_dir_all(&dir).await.is_ok() {
                deleted = true;
            }
        }
        // Remove legacy flat file if present
        let legacy = self.legacy_path(name);
        if legacy.exists() {
            if tokio::fs::remove_file(&legacy).await.is_ok() {
                deleted = true;
            }
        }
        Ok(deleted)
    }
}

// ── InMemorySkillStore ────────────────────────────────────────────────────────

/// In-memory skill store — useful for tests and WASM targets.
#[derive(Clone, Default)]
pub struct InMemorySkillStore {
    map: Arc<Mutex<HashMap<String, String>>>,
}

impl InMemorySkillStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SkillStore for InMemorySkillStore {
    async fn save(&self, name: &str, content: &str) -> Result<String, AgentError> {
        self.map
            .lock()
            .unwrap()
            .insert(name.to_string(), content.to_string());
        Ok(format!("memory:{name}"))
    }

    async fn get(&self, name: &str) -> Result<Option<String>, AgentError> {
        Ok(self.map.lock().unwrap().get(name).cloned())
    }

    async fn list(&self) -> Result<Vec<String>, AgentError> {
        let mut names: Vec<String> = self.map.lock().unwrap().keys().cloned().collect();
        names.sort();
        Ok(names)
    }

    async fn delete(&self, name: &str) -> Result<bool, AgentError> {
        Ok(self.map.lock().unwrap().remove(name).is_some())
    }
}

// ── FsSkillStore ──────────────────────────────────────────────────────────────

/// Stores each skill as `<base_path>/<name>.md` inside any `bashkit::FileSystem`.
///
/// Works with `bashkit::InMemoryFs`, `bashkit::PosixFs`, overlay filesystems,
/// or any custom implementation.  Can be paired with `FsMode::build_fs()` to
/// share the same virtual filesystem used by the agent's bash and fs tools.
#[cfg(feature = "skill-virtual")]
#[derive(Clone)]
pub struct FsSkillStore {
    fs: Arc<dyn bashkit::FileSystem>,
    base_path: String,
}

#[cfg(feature = "skill-virtual")]
impl FsSkillStore {
    /// Create a new store rooted at `base_path` inside `fs`.
    ///
    /// # Example
    /// ```ignore
    /// let mem = bashkit::InMemoryFs::new();
    /// let store = FsSkillStore::new(Arc::new(mem), "/skills");
    /// ```
    pub fn new(fs: Arc<dyn bashkit::FileSystem>, base_path: impl Into<String>) -> Self {
        Self {
            fs,
            base_path: base_path.into(),
        }
    }

    fn path_for(&self, name: &str) -> std::path::PathBuf {
        let safe = name.replace(['/', '\\', '.'], "_");
        std::path::Path::new(&self.base_path).join(format!("{safe}.md"))
    }
}

#[cfg(feature = "skill-virtual")]
fn is_not_found(e: &bashkit::Error) -> bool {
    if let bashkit::Error::Io(io_err) = e {
        io_err.kind() == std::io::ErrorKind::NotFound
    } else {
        false
    }
}

#[cfg(feature = "skill-virtual")]
impl SkillStore for FsSkillStore {
    async fn save(&self, name: &str, content: &str) -> Result<String, AgentError> {
        self.fs
            .mkdir(std::path::Path::new(&self.base_path), true)
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        let path = self.path_for(name);
        self.fs
            .write_file(&path, content.as_bytes())
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?;
        Ok(path.to_string_lossy().to_string())
    }

    async fn get(&self, name: &str) -> Result<Option<String>, AgentError> {
        let path = self.path_for(name);
        match self.fs.read_file(&path).await {
            Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
            Err(ref e) if is_not_found(e) => Ok(None),
            Err(e) => Err(AgentError::Io(e.to_string())),
        }
    }

    async fn list(&self) -> Result<Vec<String>, AgentError> {
        let base = std::path::Path::new(&self.base_path);
        match self.fs.read_dir(base).await {
            Err(ref e) if is_not_found(e) => Ok(vec![]),
            Err(e) => Err(AgentError::Io(e.to_string())),
            Ok(entries) => {
                let mut names: Vec<String> = entries
                    .into_iter()
                    .filter_map(|entry| {
                        let n = &entry.name;
                        n.ends_with(".md")
                            .then(|| n.trim_end_matches(".md").to_string())
                    })
                    .collect();
                names.sort();
                Ok(names)
            }
        }
    }

    async fn delete(&self, name: &str) -> Result<bool, AgentError> {
        let path = self.path_for(name);
        match self.fs.remove(&path, false).await {
            Ok(_) => Ok(true),
            Err(ref e) if is_not_found(e) => Ok(false),
            Err(e) => Err(AgentError::Io(e.to_string())),
        }
    }
}
