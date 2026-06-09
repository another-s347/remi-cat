pub mod store;
pub mod tools;

pub use store::{BuiltinSkill, BuiltinSkillStore, FileSkillStore, SkillStore};
pub use tools::{SkillGetTool, SkillReadResourceTool, SkillSearchTool};

use remi_agentloop::prelude::ParsedToolCall;
use std::sync::Arc;

use crate::events::SkillEvent;

/// Register standard Agent Skills tools into a registry, backed by `store`.
pub fn register_skill_tools<S: SkillStore>(
    registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry,
    store: Arc<S>,
) {
    registry.register(SkillGetTool {
        store: Arc::clone(&store),
    });
    registry.register(SkillSearchTool {
        store: Arc::clone(&store),
    });
    registry.register(SkillReadResourceTool {
        store: Arc::clone(&store),
    });
}

#[cfg(test)]
mod tests {
    use super::{register_skill_tools, BuiltinSkillStore, FileSkillStore};
    use remi_agentloop::tool::registry::{DefaultToolRegistry, ToolRegistry};
    use std::sync::Arc;

    #[test]
    fn registry_exposes_standard_skill_tools() {
        let mut registry = DefaultToolRegistry::new();
        register_skill_tools(
            &mut registry,
            Arc::new(BuiltinSkillStore::new(FileSkillStore::with_roots([]), [])),
        );

        let defs = registry.definitions(&serde_json::Value::Null);
        let names: Vec<_> = defs.into_iter().map(|def| def.function.name).collect();

        assert!(names.iter().any(|name| name == "skill__search"));
        assert!(names.iter().any(|name| name == "skill__get"));
        assert!(names.iter().any(|name| name == "skill__read_resource"));
        assert!(!names.iter().any(|name| name == "skill__save"));
        assert!(!names.iter().any(|name| name == "skill__delete"));
    }
}

/// Produce a `SkillEvent` for mutation tools.
pub fn make_skill_event(tc: &ParsedToolCall, result_str: &str) -> Option<SkillEvent> {
    let _ = (tc, result_str);
    None
}
