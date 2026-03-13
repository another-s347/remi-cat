use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{AgentError, Tool, ToolContext, ToolOutput, ToolResult};
use remi_agentloop::types::ResumePayload;
use serde_json::json;
use std::sync::Arc;

use super::store::SkillStore;

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Remove an existing YAML frontmatter block from the start of `content`.
fn strip_frontmatter(content: &str) -> &str {
    if !content.starts_with("---") {
        return content;
    }
    let rest = content["---".len()..].trim_start_matches('\n');
    if let Some(end) = rest.find("\n---") {
        let after = &rest[end + "\n---".len()..];
        after.trim_start_matches('\n')
    } else {
        content
    }
}

// ── SkillSaveTool ─────────────────────────────────────────────────────────────

pub struct SkillSaveTool<S> {
    pub(crate) store: Arc<S>,
}

impl<S: SkillStore + 'static> Tool for SkillSaveTool<S> {
    fn name(&self) -> &str {
        "skill__save"
    }
    fn description(&self) -> &str {
        "Save a reusable skill as a named markdown document. Use this to record \
        step-by-step procedures, best practices, or any knowledge worth reusing \
        in later sessions."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name":        { "type": "string", "description": "Short identifier (kebab-case, e.g. 'setup-rust-project')" },
                "description": { "type": "string", "description": "One-sentence summary of the skill" },
                "content":     { "type": "string", "description": "Markdown body of the skill document" }
            },
            "required": ["name", "content"]
        })
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| AgentError::tool("skill__save", "missing 'name'"))?
            .to_string();
        let description = arguments["description"].as_str().unwrap_or("").to_string();
        let content = arguments["content"]
            .as_str()
            .ok_or_else(|| AgentError::tool("skill__save", "missing 'content'"))?
            .to_string();

        let body = strip_frontmatter(content.trim_start());
        let full_content = if description.is_empty() {
            format!("---\nname: {name}\n---\n\n{body}")
        } else {
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}")
        };

        let store = self.store.clone();
        let path = store.save(&name, &full_content).await?;
        Ok(ToolResult::Output(stream! {
            yield ToolOutput::text(format!("Skill '{name}' saved to {path}"));
        }))
    }
}

// ── SkillGetTool ──────────────────────────────────────────────────────────────

pub struct SkillGetTool<S> {
    pub(crate) store: Arc<S>,
}

impl<S: SkillStore + 'static> Tool for SkillGetTool<S> {
    fn name(&self) -> &str {
        "skill__get"
    }
    fn description(&self) -> &str {
        "Retrieve the full markdown content of a previously saved skill by name."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill identifier" }
            },
            "required": ["name"]
        })
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| AgentError::tool("skill__get", "missing 'name'"))?
            .to_string();
        let store = self.store.clone();
        let result = match store.get(&name).await? {
            Some(content) => content,
            None => format!("Skill '{name}' not found."),
        };
        Ok(ToolResult::Output(
            stream! { yield ToolOutput::text(result); },
        ))
    }
}

// ── SkillListTool ─────────────────────────────────────────────────────────────

pub struct SkillListTool<S> {
    pub(crate) store: Arc<S>,
}

impl<S: SkillStore + 'static> Tool for SkillListTool<S> {
    fn name(&self) -> &str {
        "skill__list"
    }
    fn description(&self) -> &str {
        "List the names of all saved skills."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let store = self.store.clone();
        let names = store.list().await?;
        let result = if names.is_empty() {
            "No skills saved yet.".to_string()
        } else {
            names.join(", ")
        };
        Ok(ToolResult::Output(
            stream! { yield ToolOutput::text(result); },
        ))
    }
}

// ── SkillDeleteTool ───────────────────────────────────────────────────────────

pub struct SkillDeleteTool<S> {
    pub(crate) store: Arc<S>,
}

impl<S: SkillStore + 'static> Tool for SkillDeleteTool<S> {
    fn name(&self) -> &str {
        "skill__delete"
    }
    fn description(&self) -> &str {
        "Permanently delete a saved skill by name."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill identifier to delete" }
            },
            "required": ["name"]
        })
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| AgentError::tool("skill__delete", "missing 'name'"))?
            .to_string();
        let store = self.store.clone();
        let existed = store.delete(&name).await?;
        let result = if existed {
            format!("Skill '{name}' deleted.")
        } else {
            format!("Skill '{name}' not found.")
        };
        Ok(ToolResult::Output(
            stream! { yield ToolOutput::text(result); },
        ))
    }
}
