use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const EMBEDDED_AGENT_PROFILES: &[(&str, &str)] = &[
    (
        "default.md",
        include_str!("../../../.remi-cat/agents/default.md"),
    ),
    (
        "explorer.md",
        include_str!("../../../.remi-cat/agents/explorer.md"),
    ),
    (
        "remi_diagnostics.md",
        include_str!("../../../.remi-cat/agents/remi_diagnostics.md"),
    ),
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentModelBindings {
    #[serde(default)]
    pub primary: Option<String>,
    #[serde(default)]
    pub helper: Option<String>,
    #[serde(default)]
    pub vision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentProfile {
    #[serde(alias = "agent_id")]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub models: AgentModelBindings,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default, alias = "agents")]
    pub delegates: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<usize>,
    #[serde(default, alias = "persistent")]
    pub persistent_sessions: bool,
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

pub fn embedded_agent_profile(id: &str) -> Result<Option<AgentProfile>> {
    for (_, contents) in EMBEDDED_AGENT_PROFILES {
        let profile = AgentProfile::from_markdown(contents)?;
        if profile.id == id {
            return Ok(Some(profile));
        }
    }
    Ok(None)
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
        if profile.name.trim().is_empty() {
            profile.name = profile.id.clone();
        }
        if profile.max_turns == Some(0) {
            profile.max_turns = None;
        }
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
    use super::{embedded_agent_profile, AgentProfile};

    #[test]
    fn parses_frontmatter_and_prompt() {
        let profile = AgentProfile::from_markdown(
            r#"---
id: default
name: Remi
description: General assistant
tools:
  - search
agents:
  - coder
---
You are Remi.
"#,
        )
        .unwrap();
        assert_eq!(profile.id, "default");
        assert_eq!(profile.system_prompt, "You are Remi.");
        assert_eq!(profile.delegates, vec!["coder"]);
        assert!(profile.allows_tool("search"));
    }

    #[test]
    fn embedded_default_profile_mentions_skill_and_memory_search() {
        let profile = embedded_agent_profile("default")
            .unwrap()
            .expect("default profile should exist");
        assert!(profile
            .system_prompt
            .contains("Pinned skills are only a curated subset"));
        assert!(profile
            .system_prompt
            .contains("Before starting substantive work, search for"));
        assert!(profile.system_prompt.contains("relevant skills and memory"));
    }
}
