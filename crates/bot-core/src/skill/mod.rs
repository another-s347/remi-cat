pub mod store;
pub mod tools;

pub use store::{FileSkillStore, InMemorySkillStore, SkillStore};
pub use tools::{SkillDeleteTool, SkillGetTool, SkillSaveTool, SkillSearchTool};

use remi_agentloop::prelude::ParsedToolCall;
use std::sync::Arc;

use crate::events::SkillEvent;

/// Register all four skill tools into a registry, backed by `store`.
pub fn register_skill_tools<S: SkillStore>(
    registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry,
    store: Arc<S>,
) {
    registry.register(SkillSaveTool {
        store: Arc::clone(&store),
    });
    registry.register(SkillGetTool {
        store: Arc::clone(&store),
    });
    registry.register(SkillSearchTool {
        store: Arc::clone(&store),
    });
    registry.register(SkillDeleteTool {
        store: Arc::clone(&store),
    });
}

#[cfg(test)]
mod tests {
    use super::{register_skill_tools, InMemorySkillStore};
    use remi_agentloop::tool::registry::{DefaultToolRegistry, ToolRegistry};
    use std::sync::Arc;

    #[test]
    fn registry_exposes_search_and_not_list() {
        let mut registry = DefaultToolRegistry::new();
        register_skill_tools(&mut registry, Arc::new(InMemorySkillStore::new()));

        let defs = registry.definitions(&serde_json::Value::Null);
        let names: Vec<_> = defs.into_iter().map(|def| def.function.name).collect();

        assert!(names.iter().any(|name| name == "skill__search"));
        assert!(!names.iter().any(|name| name == "skill__list"));
    }
}

/// Produce a `SkillEvent` for mutation tools (save/delete).
pub fn make_skill_event(tc: &ParsedToolCall) -> Option<SkillEvent> {
    match tc.name.as_str() {
        "skill__save" => {
            let name = tc.arguments["name"].as_str()?.to_string();
            Some(SkillEvent::Saved {
                path: format!(".skills/{name}.md"),
                name,
            })
        }
        "skill__delete" => {
            let name = tc.arguments["name"].as_str()?.to_string();
            Some(SkillEvent::Deleted { name })
        }
        _ => None,
    }
}
