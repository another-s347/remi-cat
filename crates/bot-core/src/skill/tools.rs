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

pub const READ_SKILLS_STATE_KEY: &str = "__read_skills";
const LEGACY_ACTIVATED_SKILLS_STATE_KEY: &str = "__activated_skills";

fn render_skill_summary(summary: &SkillSummary) -> String {
    format!(
        "- {} - {} ({})",
        summary.name, summary.description, summary.source
    )
}

fn render_skill_get_extra_prompt(skills: &[SkillSummary]) -> String {
    if skills.is_empty() {
        return "Local skills follow the Agent Skills SKILL.md directory format. Use skill__search with keywords to discover skills by name and description. Use skill__get to read one skill's instructions for the current turn; reading a skill also makes its resources available through skill__read_resource.".to_string();
    }

    let mut prompt = String::from(
        "Available local skills (catalog only; full instructions are loaded by skill__get):\n",
    );
    for skill in skills {
        prompt.push_str(&render_skill_summary(skill));
        prompt.push('\n');
    }
    prompt.push_str(
        "Use skill__search to discover additional skills. Use skill__get with the exact skill name to read one skill's instructions for the current turn.",
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

pub fn read_skill_names(user_state: &serde_json::Value) -> Vec<String> {
    let mut names = Vec::new();
    for key in [READ_SKILLS_STATE_KEY, LEGACY_ACTIVATED_SKILLS_STATE_KEY] {
        if let Some(items) = user_state.get(key).and_then(serde_json::Value::as_array) {
            for name in items.iter().filter_map(serde_json::Value::as_str) {
                if !names.iter().any(|seen| seen == name) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

fn mark_skill_read(ctx: &ToolContext, name: &str) {
    let mut state = ctx.user_state.write().unwrap();
    if !state.is_object() {
        *state = json!({});
    }
    let Some(map) = state.as_object_mut() else {
        return;
    };
    let entry = map
        .entry(READ_SKILLS_STATE_KEY.to_string())
        .or_insert_with(|| json!([]));
    if !entry.is_array() {
        *entry = json!([]);
    }
    let Some(items) = entry.as_array_mut() else {
        return;
    };
    if !items
        .iter()
        .any(|item| item.as_str().is_some_and(|value| value == name))
    {
        items.push(serde_json::Value::String(name.to_string()));
    }
}

fn skill_was_read(ctx: &ToolContext, name: &str) -> bool {
    read_skill_names(&ctx.user_state.read().unwrap())
        .iter()
        .any(|item| item == name)
}

pub struct SkillGetTool<S> {
    pub(crate) store: Arc<S>,
}

impl<S: SkillStore + 'static> Tool for SkillGetTool<S> {
    fn name(&self) -> &str {
        "skill__get"
    }

    fn description(&self) -> &str {
        "Read the full SKILL.md instructions for a local skill by name. The instructions are returned in this tool result; reading a skill also enables skill__read_resource for files in that skill."
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
                "name": { "type": "string", "description": "Skill name from the catalog" }
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
            match self.store.get(&name).await? {
                Some(doc) => {
                    mark_skill_read(ctx, &doc.name);
                    format!(
                        "{}\n\n[skill read: {} from {}; resources available via skill__read_resource]",
                        doc.content.trim_end(),
                        doc.name,
                        doc.source
                    )
                }
                None => format!("Skill '{name}' not found."),
            }
        };
        Ok(ToolResult::Output(
            stream! { yield ToolOutput::text(result); },
        ))
    }
}

pub struct SkillSearchTool<S> {
    pub(crate) store: Arc<S>,
}

impl<S: SkillStore + 'static> Tool for SkillSearchTool<S> {
    fn name(&self) -> &str {
        "skill__search"
    }

    fn description(&self) -> &str {
        "Search the local skill catalog by keyword across skill names and descriptions. Results contain only name, description, and source; use skill__get to read a skill."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "One or more keywords separated by spaces. Empty query lists the catalog."
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
        let matches = filter_trigger_skill(
            self.store.search(&query).await?,
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
            text.push_str("Use skill__get with the exact skill name to read one.");
            text
        };
        Ok(ToolResult::Output(
            stream! { yield ToolOutput::text(result); },
        ))
    }
}

pub struct SkillReadResourceTool<S> {
    pub(crate) store: Arc<S>,
}

impl<S: SkillStore + 'static> Tool for SkillReadResourceTool<S> {
    fn name(&self) -> &str {
        "skill__read_resource"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text resource from a skill directory after that skill has been read with skill__get. Use this for scripts, templates, and reference files mentioned by SKILL.md."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Skill name previously read with skill__get" },
                "path": { "type": "string", "description": "Relative path inside the skill directory" }
            },
            "required": ["skill", "path"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let skill = arguments["skill"]
            .as_str()
            .ok_or_else(|| AgentError::tool("skill__read_resource", "missing 'skill'"))?
            .to_string();
        let path = arguments["path"]
            .as_str()
            .ok_or_else(|| AgentError::tool("skill__read_resource", "missing 'path'"))?
            .to_string();
        let result = if !skill_was_read(ctx, &skill) {
            format!(
                "Skill '{skill}' has not been read in this session. Read it first with skill__get."
            )
        } else {
            let resource = self.store.read_resource(&skill, &path).await?;
            if resource.binary {
                format!(
                    "Resource '{}' in skill '{}' is binary ({} bytes); binary content was not inlined.",
                    resource.path, resource.skill, resource.size
                )
            } else {
                resource.content.unwrap_or_default()
            }
        };
        Ok(ToolResult::Output(
            stream! { yield ToolOutput::text(result); },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        filter_trigger_skill, read_skill_names, render_skill_get_extra_prompt, SkillGetTool,
    };
    use crate::skill::store::{BuiltinSkill, BuiltinSkillStore, FileSkillStore, SkillSummary};
    use futures::StreamExt;
    use remi_agentloop::prelude::{
        AgentConfig, Tool, ToolContext, ToolDefinitionContext, ToolOutput, ToolResult,
    };
    use std::sync::{Arc, RwLock};

    #[test]
    fn empty_featured_prompt_points_to_search_and_get() {
        let prompt = render_skill_get_extra_prompt(&Vec::<SkillSummary>::new());
        assert!(prompt.contains("skill__search"));
        assert!(prompt.contains("skill__get"));
        assert!(prompt.contains("current turn"));
        assert!(prompt.contains("skill__read_resource"));
    }

    #[test]
    fn filter_trigger_skill_removes_builtin_entry_when_suppressed() {
        let filtered = filter_trigger_skill(
            vec![
                SkillSummary {
                    name: "trigger".to_string(),
                    description: "builtin trigger skill".to_string(),
                    source: "builtin".to_string(),
                },
                SkillSummary {
                    name: "rust-build".to_string(),
                    description: "build help".to_string(),
                    source: ".remi-cat/skills".to_string(),
                },
            ],
            true,
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "rust-build");
    }

    #[test]
    fn skill_get_extra_prompt_lists_catalog_without_body() {
        let store = Arc::new(BuiltinSkillStore::new(
            FileSkillStore::with_roots([]),
            [BuiltinSkill {
                name: "trigger",
                description: "Builtin trigger capability reference",
                content: "---\nname: trigger\ndescription: Builtin trigger capability reference\n---\n\nSecret body",
            }],
        ));
        let tool = SkillGetTool { store };
        let prompt = tool
            .extra_prompt(&ToolDefinitionContext::default())
            .expect("skill__get should always advertise discovery guidance");

        assert!(prompt.contains("trigger"));
        assert!(prompt.contains("Builtin trigger capability reference"));
        assert!(!prompt.contains("Secret body"));
    }

    #[test]
    fn read_skill_names_reads_current_and_legacy_session_state() {
        let state = serde_json::json!({
            "__read_skills": ["docs", "researcher"],
            "__activated_skills": ["researcher", "legacy"]
        });
        assert_eq!(
            read_skill_names(&state),
            vec!["docs", "researcher", "legacy"]
        );
    }

    #[tokio::test]
    async fn skill_get_returns_instruction_as_tool_result_and_marks_resource_access() {
        let store = Arc::new(BuiltinSkillStore::new(
            FileSkillStore::with_roots([]),
            [BuiltinSkill {
                name: "researcher",
                description: "Research workflow",
                content: "---\nname: researcher\ndescription: Research workflow\n---\n\nUse primary sources.",
            }],
        ));
        let tool = SkillGetTool { store };
        let ctx = ToolContext {
            config: AgentConfig::default(),
            thread_id: Some(
                serde_json::from_value(serde_json::json!("test-thread"))
                    .expect("thread_id should deserialize"),
            ),
            run_id: serde_json::from_value(serde_json::json!("test-run"))
                .expect("run_id should deserialize"),
            metadata: None,
            user_state: Arc::new(RwLock::new(serde_json::Value::Null)),
        };

        let result = tool
            .execute(serde_json::json!({ "name": "researcher" }), None, &ctx)
            .await
            .expect("skill__get should succeed");
        let ToolResult::Output(stream) = result else {
            panic!("expected tool output");
        };
        let mut stream = std::pin::pin!(stream);
        let output = match stream.next().await {
            Some(ToolOutput::Result(content)) => content.text_content(),
            other => panic!("expected text output, got {other:?}"),
        };

        assert!(output.contains("Use primary sources."));
        assert!(output.contains("[skill read: researcher"));
        assert!(!output.contains("[skill activated:"));
        assert_eq!(
            read_skill_names(&ctx.user_state.read().unwrap()),
            vec!["researcher"]
        );
    }
}
