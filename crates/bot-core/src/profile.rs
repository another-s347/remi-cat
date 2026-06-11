use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const EMBEDDED_AGENT_PROFILES: &[(&str, &str)] = &[(
    "default.md",
    include_str!("../../../.remi-cat/agents/default.md"),
)];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentModelBindings {
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default)]
    pub helper: Option<String>,
    #[serde(default)]
    pub vision: Option<String>,
}

impl Default for AgentModelBindings {
    fn default() -> Self {
        Self {
            primary: None,
            helper: None,
            vision: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub models: AgentModelBindings,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub delegates: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<usize>,
    #[serde(skip)]
    pub system_prompt: String,
}

#[derive(Debug, Clone)]
pub struct AgentRegistry {
    dir: PathBuf,
    profiles: HashMap<String, AgentProfile>,
}

pub fn install_embedded_agent_profiles(dir: impl AsRef<Path>) -> Result<()> {
    let dir = dir.as_ref();
    let has_markdown = dir.exists()
        && std::fs::read_dir(dir)
            .with_context(|| format!("reading agent profile dir {}", dir.display()))?
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("md"));
    if has_markdown {
        return Ok(());
    }

    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating agent profile dir {}", dir.display()))?;
    for (name, contents) in EMBEDDED_AGENT_PROFILES {
        let path = dir.join(name);
        if !path.exists() {
            std::fs::write(&path, contents)
                .with_context(|| format!("writing agent profile {}", path.display()))?;
        }
    }
    Ok(())
}

impl AgentRegistry {
    pub fn load(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        let mut profiles = HashMap::new();
        if dir.exists() {
            for entry in std::fs::read_dir(&dir)
                .with_context(|| format!("reading agent profile dir {}", dir.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("md") {
                    continue;
                }
                let profile = AgentProfile::from_markdown_file(&path)?;
                profiles.insert(profile.id.clone(), profile);
            }
        }
        Ok(Self { dir, profiles })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn get(&self, id: &str) -> Option<&AgentProfile> {
        self.profiles.get(id)
    }

    pub fn profiles(&self) -> impl Iterator<Item = &AgentProfile> {
        self.profiles.values()
    }

    pub fn upsert_markdown(&mut self, file_name: &str, markdown: &str) -> Result<AgentProfile> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating agent profile dir {}", self.dir.display()))?;
        let path = self.dir.join(file_name);
        std::fs::write(&path, markdown)
            .with_context(|| format!("writing agent profile {}", path.display()))?;
        let profile = AgentProfile::from_markdown(markdown)
            .with_context(|| format!("parsing agent profile {}", path.display()))?;
        self.profiles.insert(profile.id.clone(), profile.clone());
        Ok(profile)
    }
}

impl AgentProfile {
    pub fn from_markdown_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading agent profile {}", path.display()))?;
        Self::from_markdown(&raw)
            .with_context(|| format!("parsing agent profile {}", path.display()))
    }

    pub fn from_markdown(raw: &str) -> Result<Self> {
        let (frontmatter, body) = split_frontmatter(raw)?;
        let mut profile: AgentProfile =
            serde_yaml::from_str(frontmatter).context("invalid agent YAML frontmatter")?;
        validate_required("id", &profile.id)?;
        validate_required("name", &profile.name)?;
        validate_required("description", &profile.description)?;
        profile.system_prompt = body.trim().to_string();
        if profile.system_prompt.is_empty() {
            anyhow::bail!("agent profile system prompt body must not be empty");
        }
        Ok(profile)
    }

    pub fn allows_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool == name)
    }
}

fn split_frontmatter(raw: &str) -> Result<(&str, &str)> {
    let raw = raw
        .strip_prefix("---")
        .context("agent profile must start with YAML frontmatter")?;
    let raw = raw.strip_prefix('\n').unwrap_or(raw);
    let Some((frontmatter, body)) = raw.split_once("\n---") else {
        anyhow::bail!("agent profile frontmatter must be closed with ---");
    };
    let body = body.strip_prefix('\n').unwrap_or(body);
    Ok((frontmatter, body))
}

fn validate_required(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("agent profile `{field}` must not be empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{install_embedded_agent_profiles, AgentProfile};

    const PROFILE: &str = r#"---
id: default
name: Remi
description: General assistant
model: gpt-4o
models:
  helper: deepseek-v4-flash
  vision: gpt-4o
tools:
  - search
  - acp__chat
delegates:
  - coder
max_turns: 12
---
You are Remi.
"#;

    #[test]
    fn parses_frontmatter_and_prompt() {
        let profile = AgentProfile::from_markdown(PROFILE).unwrap();
        assert_eq!(profile.id, "default");
        assert_eq!(profile.system_prompt, "You are Remi.");
        assert_eq!(profile.models.helper.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(profile.models.vision.as_deref(), Some("gpt-4o"));
        assert!(profile.allows_tool("acp__chat"));
        assert!(!profile.allows_tool("bash"));
    }

    #[test]
    fn requires_identity_fields() {
        let err = AgentProfile::from_markdown("---\nid: x\nname: ''\ndescription: y\n---\nbody")
            .unwrap_err()
            .to_string();
        assert!(err.contains("name"));
    }

    #[test]
    fn installs_embedded_default_agent_into_empty_dir() {
        let dir = std::env::temp_dir().join(format!("remi-agent-seed-{}", uuid::Uuid::new_v4()));
        install_embedded_agent_profiles(&dir).unwrap();
        let path = dir.join("default.md");
        assert!(path.exists());
        let profile = AgentProfile::from_markdown_file(&path).unwrap();
        assert_eq!(profile.id, "default");
        assert!(profile.allows_tool("fetch"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
