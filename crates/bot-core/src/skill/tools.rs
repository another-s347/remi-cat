use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{
    AgentError, Tool, ToolContext, ToolDefinitionContext, ToolOutput, ToolResult,
};
use remi_agentloop::types::ResumePayload;
use serde_json::json;
use std::sync::Arc;

use super::store::{SkillStore, SkillSummary};
use crate::{suppress_trigger_management, trigger::BUILTIN_TRIGGER_SKILL_NAME};

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

fn render_skill_summary(summary: &SkillSummary) -> String {
    match summary.description.as_deref() {
        Some(description) => format!("- {} — {}", summary.name, description),
        None => format!("- {} — (no description)", summary.name),
    }
}

fn render_skill_get_extra_prompt(skills: &[SkillSummary]) -> String {
    if skills.is_empty() {
        return "You can use skill__search with one or more keywords to discover local skills by name and description, including builtin read-only skills.".to_string();
    }

    let mut prompt = String::from(
        "Recent local skill snapshot (cached for up to 30 minutes; ordered by recent successful skill__get):\n",
    );
    for skill in skills {
        prompt.push_str(&render_skill_summary(skill));
        prompt.push('\n');
    }
    prompt.push_str(
        "Use skill__search with one or more keywords to discover additional local skills beyond this snapshot, including builtin read-only skills.",
    );
    prompt
}

fn filter_trigger_skill(skills: Vec<SkillSummary>, suppress: bool) -> Vec<SkillSummary> {
    if !suppress {
        return skills;
    }

    skills
        .into_iter()
        .filter(|skill| skill.name != BUILTIN_TRIGGER_SKILL_NAME)
        .collect()
}

fn hides_trigger_skill(name: &str, suppress: bool) -> bool {
    suppress && name.trim() == BUILTIN_TRIGGER_SKILL_NAME
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
        in later sessions. Builtin skill names are reserved and cannot be overwritten."
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
        "Retrieve the full markdown content of a local skill by name, including builtin read-only skills."
    }
    fn extra_prompt(&self, ctx: &ToolDefinitionContext) -> Option<String> {
        let skills = filter_trigger_skill(
            self.store.featured_summaries(),
            suppress_trigger_management(ctx.metadata.as_ref()),
        );
        Some(render_skill_get_extra_prompt(&skills))
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
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let name = arguments["name"]
            .as_str()
            .ok_or_else(|| AgentError::tool("skill__get", "missing 'name'"))?
            .to_string();
        let suppress = suppress_trigger_management(ctx.metadata.as_ref());
        let result = if hides_trigger_skill(&name, suppress) {
            format!("Skill '{name}' not found.")
        } else {
            let store = self.store.clone();
            match store.get(&name).await? {
                Some(content) => content,
                None => format!("Skill '{name}' not found."),
            }
        };
        Ok(ToolResult::Output(
            stream! { yield ToolOutput::text(result); },
        ))
    }
}

// ── SkillSearchTool ───────────────────────────────────────────────────────────

pub struct SkillSearchTool<S> {
    pub(crate) store: Arc<S>,
}

impl<S: SkillStore + 'static> Tool for SkillSearchTool<S> {
    fn name(&self) -> &str {
        "skill__search"
    }
    fn description(&self) -> &str {
        "Search local skills by keyword across skill names and descriptions. Accepts multiple keywords and favors recall over strict matching. Results may include builtin read-only skills and user-saved skills."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "One or more keywords separated by spaces. Search matches skill names and descriptions with OR semantics."
                }
            },
            "required": ["query"]
        })
    }
    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let query = arguments["query"]
            .as_str()
            .ok_or_else(|| AgentError::tool("skill__search", "missing 'query'"))?
            .trim()
            .to_string();
        let store = self.store.clone();
        let matches = filter_trigger_skill(
            store.search(&query).await?,
            suppress_trigger_management(ctx.metadata.as_ref()),
        );
        let result = if matches.is_empty() {
            format!(
                "No skills matched '{query}'. Search checks skill names and descriptions; try broader keywords."
            )
        } else {
            let mut text = format!("Found {} matching skill(s) for '{query}':\n", matches.len());
            for skill in matches {
                text.push_str(&render_skill_summary(&skill));
                text.push('\n');
            }
            text.push_str("Use skill__get with the exact skill name to open one.");
            text
        };
        Ok(ToolResult::Output(
            stream! { yield ToolOutput::text(result); },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{filter_trigger_skill, render_skill_get_extra_prompt, SkillGetTool};
    use crate::skill::store::{InMemorySkillStore, SkillStore, SkillSummary};
    use remi_agentloop::prelude::{Tool, ToolDefinitionContext};
    use std::sync::Arc;

    #[tokio::test]
    async fn skill_get_extra_prompt_lists_at_most_eight_skills_and_search_hint() {
        let store = Arc::new(InMemorySkillStore::new());
        for index in 0..9 {
            let name = format!("skill-{index}");
            let content =
                format!("---\nname: {name}\ndescription: Description {index}\n---\n\nBody {index}");
            store.save(&name, &content).await.unwrap();
        }

        let tool = SkillGetTool {
            store: Arc::clone(&store),
        };
        let prompt = tool
            .extra_prompt(&ToolDefinitionContext::default())
            .expect("skill__get should always advertise discovery guidance");

        assert!(prompt.contains("skill__search"));
        assert_eq!(
            prompt.lines().filter(|line| line.starts_with("- ")).count(),
            8
        );
        assert!(!prompt.contains("skill-0 — Description 0"));
        assert!(prompt.contains("skill-8 — Description 8"));
    }

    #[test]
    fn empty_featured_prompt_still_points_to_search() {
        let prompt = render_skill_get_extra_prompt(&Vec::<SkillSummary>::new());
        assert!(prompt.contains("skill__search"));
    }

    #[test]
    fn filter_trigger_skill_removes_builtin_entry_when_suppressed() {
        let filtered = filter_trigger_skill(
            vec![
                SkillSummary {
                    name: "trigger".to_string(),
                    description: Some("builtin trigger skill".to_string()),
                },
                SkillSummary {
                    name: "rust-build".to_string(),
                    description: Some("build help".to_string()),
                },
            ],
            true,
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "rust-build");
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
        "Permanently delete a saved user skill by name. Builtin skills cannot be deleted."
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
