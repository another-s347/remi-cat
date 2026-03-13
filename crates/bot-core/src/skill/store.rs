use remi_agentloop::prelude::AgentError;
use std::path::PathBuf;

// ── SkillStore trait ─────────────────────────────────────────────────────────

/// Persistent backend for named skill documents.
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

// ── FileSkillStore ───────────────────────────────────────────────────────────

/// Stores each skill as `<base_dir>/<name>.md`.
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

    fn skill_path(&self, name: &str) -> PathBuf {
        // Sanitize to prevent path traversal
        let safe = name.replace(['/', '\\', '.'], "_");
        self.base_dir.join(format!("{safe}.md"))
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
        Ok(path.to_string_lossy().to_string())
    }

    async fn get(&self, name: &str) -> Result<Option<String>, AgentError> {
        let path = self.skill_path(name);
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AgentError::Io(e.to_string())),
        }
    }

    async fn list(&self) -> Result<Vec<String>, AgentError> {
        let mut names = Vec::new();
        let mut rd = match tokio::fs::read_dir(&self.base_dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(AgentError::Io(e.to_string())),
        };
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| AgentError::Io(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
        names.sort();
        Ok(names)
    }

    async fn delete(&self, name: &str) -> Result<bool, AgentError> {
        let path = self.skill_path(name);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(AgentError::Io(e.to_string())),
        }
    }
}

// ── InMemorySkillStore ───────────────────────────────────────────────────────

/// In-memory skill store for testing.
#[derive(Default)]
pub struct InMemorySkillStore {
    data: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl InMemorySkillStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SkillStore for InMemorySkillStore {
    async fn save(&self, name: &str, content: &str) -> Result<String, AgentError> {
        self.data
            .lock()
            .unwrap()
            .insert(name.to_string(), content.to_string());
        Ok(format!("memory:{name}"))
    }

    async fn get(&self, name: &str) -> Result<Option<String>, AgentError> {
        Ok(self.data.lock().unwrap().get(name).cloned())
    }

    async fn list(&self) -> Result<Vec<String>, AgentError> {
        let mut names: Vec<String> = self.data.lock().unwrap().keys().cloned().collect();
        names.sort();
        Ok(names)
    }

    async fn delete(&self, name: &str) -> Result<bool, AgentError> {
        Ok(self.data.lock().unwrap().remove(name).is_some())
    }
}
