use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{
    AgentError, Tool, ToolContext, ToolDefinitionContext, ToolOutput, ToolResult,
};
use remi_agentloop::types::ResumePayload;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

use super::store::{
    canonical_name, SkillLoadDiagnostic, SkillLoadDiagnosticSeverity, SkillStore, SkillSummary,
};
use crate::{suppress_trigger_management, trigger::BUILTIN_TRIGGER_SKILL_NAME};

pub const READ_SKILLS_STATE_KEY: &str = "__read_skills";
const LEGACY_ACTIVATED_SKILLS_STATE_KEY: &str = "__activated_skills";

fn render_skill_summary(summary: &SkillSummary) -> String {
    format!(
        "- {} - {} ({})",
        summary.name, summary.description, summary.source
    )
}

pub fn render_skill_diagnostics_summary(diagnostics: &[SkillLoadDiagnostic]) -> String {
    if diagnostics.is_empty() {
        return String::new();
    }
    let mut text = String::from("\n\nSkill load diagnostics:\n");
    let limit = 8;
    for diagnostic in diagnostics.iter().take(limit) {
        let severity = match diagnostic.severity {
            SkillLoadDiagnosticSeverity::Warning => "warning",
            SkillLoadDiagnosticSeverity::Error => "error",
        };
        let skill = diagnostic
            .skill
            .as_deref()
            .map(|skill| format!(" skill={skill}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "- {severity}:{skill} {} - {}\n",
            diagnostic.path, diagnostic.message
        ));
    }
    if diagnostics.len() > limit {
        text.push_str(&format!(
            "- ... {} more diagnostic(s) omitted\n",
            diagnostics.len() - limit
        ));
    }
    text.trim_end().to_string()
}

fn diagnostics_for_skill_name(
    diagnostics: &[SkillLoadDiagnostic],
    name: &str,
) -> Vec<SkillLoadDiagnostic> {
    let requested = canonical_name(name);
    diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .skill
                .as_deref()
                .map(canonical_name)
                .is_some_and(|skill| skill == requested)
                || diagnostic_path_skill_name(&diagnostic.path)
                    .map(|skill| skill == requested)
                    .unwrap_or(false)
        })
        .cloned()
        .collect()
}

fn diagnostic_path_skill_name(path: &str) -> Option<String> {
    let skill_dir = Path::new(path).parent()?.file_name()?.to_str()?;
    Some(canonical_name(skill_dir))
}

fn render_skill_get_extra_prompt(skills: &[SkillSummary]) -> String {
    if skills.is_empty() {
        return "Local skills follow the Agent Skills SKILL.md directory format. Use skill__search with keywords to discover skills by name and description. Use skill__get to read one skill's instructions for the current turn; use fs_read with the returned resource_root_path for supporting files.".to_string();
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

pub struct SkillGetTool<S> {
    pub(crate) store: Arc<S>,
}

impl<S: SkillStore + 'static> Tool for SkillGetTool<S> {
    fn name(&self) -> &str {
        "skill__get"
    }

    fn description(&self) -> &str {
        "Read the full SKILL.md instructions for a local skill by name. The instructions are returned in this tool result. For supporting files, use fs_read with the returned resource_root_path."
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
                    let diagnostics =
                        diagnostics_for_skill_name(&self.store.load_diagnostics(), &doc.name);
                    let resource_hint = match (&doc.skill_file_path, &doc.resource_root_path) {
                        (Some(skill_file), Some(resource_root)) => format!(
                            "fs_read skill_file_path={skill_file}; resource_root_path={resource_root}"
                        ),
                        _ => "resources are not readable through fs_read because this skill is outside the workspace root".to_string(),
                    };
                    let mut text = format!(
                        "{}\n\n[skill read: {} from {}; {}]",
                        doc.content.trim_end(),
                        doc.name,
                        doc.source,
                        resource_hint
                    );
                    text.push_str(&render_skill_diagnostics_summary(&diagnostics));
                    text
                }
                None => {
                    let diagnostics = self.store.load_diagnostics();
                    let relevant = diagnostics_for_skill_name(&diagnostics, &name);
                    let diagnostics = if relevant.is_empty() {
                        diagnostics
                    } else {
                        relevant
                    };
                    format!(
                        "Skill '{name}' not found.{}",
                        render_skill_diagnostics_summary(&diagnostics)
                    )
                }
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
        let diagnostics = self.store.load_diagnostics();
        let result = if matches.is_empty() {
            format!(
                "No skills matched '{query}'. Search checks skill names and descriptions; try broader keywords.{}",
                render_skill_diagnostics_summary(&diagnostics)
            )
        } else {
            let mut text = format!("Found {} matching skill(s) for '{query}':\n", matches.len());
            for skill in matches {
                text.push_str(&render_skill_summary(&skill));
                text.push('\n');
            }
            text.push_str("Use skill__get with the exact skill name to read one.");
            text.push_str(&render_skill_diagnostics_summary(&diagnostics));
            text
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
        SkillSearchTool,
    };
    use crate::skill::store::{BuiltinSkill, BuiltinSkillStore, FileSkillStore, SkillSummary};
    use futures::StreamExt;
    use remi_agentloop::prelude::{
        AgentConfig, Tool, ToolContext, ToolDefinitionContext, ToolOutput, ToolResult,
    };
    use std::sync::{Arc, RwLock};
    use uuid::Uuid;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("remi-skill-tool-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn empty_featured_prompt_points_to_search_and_get() {
        let prompt = render_skill_get_extra_prompt(&Vec::<SkillSummary>::new());
        assert!(prompt.contains("skill__search"));
        assert!(prompt.contains("skill__get"));
        assert!(prompt.contains("current turn"));
        assert!(prompt.contains("fs_read"));
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
                name: "docs",
                description: "Builtin docs capability reference",
                content:
                    "---\nname: docs\ndescription: Builtin docs capability reference\n---\n\nSecret body"
                        .to_string(),
            }],
        ));
        let tool = SkillGetTool { store };
        let prompt = tool
            .extra_prompt(&ToolDefinitionContext::default())
            .expect("skill__get should always advertise discovery guidance");

        assert!(prompt.contains("docs"));
        assert!(prompt.contains("Builtin docs capability reference"));
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
                content: "---\nname: researcher\ndescription: Research workflow\n---\n\nUse primary sources.".to_string(),
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
        assert!(output.contains("resources are not readable through fs_read"));
        assert!(!output.contains("[skill activated:"));
        assert_eq!(
            read_skill_names(&ctx.user_state.read().unwrap()),
            vec!["researcher"]
        );
    }

    #[tokio::test]
    async fn skill_get_returns_fs_read_resource_paths_for_file_skills() {
        let workspace = temp_dir();
        let primary = workspace.join(".remi-cat").join("skills");
        let skill_dir = primary.join("docs");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: docs\ndescription: Docs workflow\n---\n\nRead references.",
        )
        .unwrap();
        std::fs::write(skill_dir.join("reference.md"), "details").unwrap();
        let store = Arc::new(BuiltinSkillStore::new(
            FileSkillStore::new_in_workspace(&primary, &workspace),
            [],
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
            .execute(serde_json::json!({ "name": "docs" }), None, &ctx)
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

        assert!(output.contains("skill_file_path=.remi-cat/skills/docs/SKILL.md"));
        assert!(output.contains("resource_root_path=.remi-cat/skills/docs"));

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn skill_get_includes_relevant_warnings_for_leniently_loaded_skill() {
        let workspace = temp_dir();
        let primary = workspace.join(".remi-cat").join("skills");
        let skill_dir = primary.join("Loose Skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Loose workflow\n\nUse it.").unwrap();
        let store = Arc::new(BuiltinSkillStore::new(
            FileSkillStore::new_in_workspace(&primary, &workspace),
            [],
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
            .execute(serde_json::json!({ "name": "loose-skill" }), None, &ctx)
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

        assert!(output.contains("[skill read: loose-skill"));
        assert!(output.contains("Skill load diagnostics"));
        assert!(output.contains("skill=loose-skill"));
        assert!(output.contains("missing YAML frontmatter"));

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn skill_get_returns_diagnostics_for_failed_skill_by_directory_name() {
        let workspace = temp_dir();
        let primary = workspace.join(".remi-cat").join("skills");
        let skill_dir = primary.join("broken");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: [broken\ndescription: Bad\n---\n\nBody",
        )
        .unwrap();
        let store = Arc::new(BuiltinSkillStore::new(
            FileSkillStore::new_in_workspace(&primary, &workspace),
            [],
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
            .execute(serde_json::json!({ "name": "broken" }), None, &ctx)
            .await
            .expect("skill__get should return diagnostics");
        let ToolResult::Output(stream) = result else {
            panic!("expected tool output");
        };
        let mut stream = std::pin::pin!(stream);
        let output = match stream.next().await {
            Some(ToolOutput::Result(content)) => content.text_content(),
            other => panic!("expected text output, got {other:?}"),
        };

        assert!(output.contains("Skill 'broken' not found"));
        assert!(output.contains("Skill load diagnostics"));
        assert!(output.contains("skill=broken"));
        assert!(output.contains("invalid skill frontmatter"));

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn skill_search_includes_load_diagnostics() {
        let primary = temp_dir();
        let compat = temp_dir();
        let dir = primary.join("Loose Skill");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "# Loose workflow\n\nUse it.").unwrap();
        let store = Arc::new(BuiltinSkillStore::new(
            FileSkillStore::with_roots([
                (primary.clone(), ".remi-cat/skills".to_string()),
                (compat.clone(), ".agents/skills".to_string()),
            ]),
            [],
        ));
        let tool = SkillSearchTool { store };
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
            .execute(serde_json::json!({ "query": "loose" }), None, &ctx)
            .await
            .expect("skill__search should succeed");
        let ToolResult::Output(stream) = result else {
            panic!("expected tool output");
        };
        let mut stream = std::pin::pin!(stream);
        let output = match stream.next().await {
            Some(ToolOutput::Result(content)) => content.text_content(),
            other => panic!("expected text output, got {other:?}"),
        };

        assert!(output.contains("loose-skill"));
        assert!(output.contains("Skill load diagnostics"));
        assert!(output.contains("missing YAML frontmatter"));

        let _ = std::fs::remove_dir_all(primary);
        let _ = std::fs::remove_dir_all(compat);
    }
}
