pub mod store;
pub mod tools;

pub use store::{FileSkillStore, InMemorySkillStore, SkillStore};
pub use tools::{SkillDeleteTool, SkillGetTool, SkillListTool, SkillSaveTool};

use remi_agentloop::prelude::{AgentError, ParsedToolCall};
use std::sync::Arc;

use crate::events::SkillEvent;

/// Register all four skill tools into a registry, backed by `store`.
pub fn register_skill_tools<S: SkillStore>(
    registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry,
    store: Arc<S>,
) {
    registry.register(SkillSaveTool   { store: Arc::clone(&store) });
    registry.register(SkillGetTool    { store: Arc::clone(&store) });
    registry.register(SkillListTool   { store: Arc::clone(&store) });
    registry.register(SkillDeleteTool { store: Arc::clone(&store) });
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
