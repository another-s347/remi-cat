use crate::skill::BuiltinSkill;

pub const BUILTIN_DESKTOP_HTML_SKILL_NAME: &str = "desktop-html";

const BUILTIN_DESKTOP_HTML_SKILL_DESCRIPTION: &str =
    "Desktop-only protocol for self-contained HTML visualizations; load it only when the output context declares html and a visual would materially help.";

const BUILTIN_DESKTOP_HTML_SKILL: &str = include_str!("../skills/desktop-html/SKILL.md");

pub fn builtin_desktop_html_skill() -> BuiltinSkill {
    BuiltinSkill {
        name: BUILTIN_DESKTOP_HTML_SKILL_NAME,
        description: BUILTIN_DESKTOP_HTML_SKILL_DESCRIPTION,
        content: BUILTIN_DESKTOP_HTML_SKILL.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::builtin_desktop_html_skill;
    use crate::skill::{BuiltinSkillStore, FileSkillStore, SkillStore};

    #[tokio::test]
    async fn builtin_desktop_html_skill_is_searchable_and_readable() {
        let store = BuiltinSkillStore::new(
            FileSkillStore::with_roots([]),
            [builtin_desktop_html_skill()],
        );

        let results = store.search("desktop html visualization").await.unwrap();
        assert!(results.iter().any(|skill| skill.name == "desktop-html"));

        let doc = store.get("desktop-html").await.unwrap().unwrap();
        assert!(doc.content.contains("```remi-html"));
        assert!(doc.content.contains("has `html` in"));
    }
}
