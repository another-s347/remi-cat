pub mod backend;
pub mod skill;
pub mod tools;

pub use backend::TriggerBackend;
pub use skill::{builtin_trigger_skill, BUILTIN_TRIGGER_SKILL_NAME};
pub use tools::{
    TriggerDeleteTool, TriggerItem, TriggerListTool, TriggerRuleSpec, TriggerUpsertTool,
};

use remi_agentloop::prelude::ParsedToolCall;
use serde::Deserialize;
use std::sync::Arc;

use crate::events::TriggerEvent;

pub fn register_trigger_tools(
    registry: &mut remi_agentloop::tool::registry::DefaultToolRegistry,
    backend: Arc<TriggerBackend>,
) {
    registry.register(TriggerUpsertTool::new(Arc::clone(&backend)));
    registry.register(TriggerListTool::new(Arc::clone(&backend)));
    registry.register(TriggerDeleteTool::new(backend));
}

#[derive(Debug, Deserialize)]
struct TriggerUpsertEventResult {
    item: TriggerUpsertEventItem,
}

#[derive(Debug, Deserialize)]
struct TriggerUpsertEventItem {
    id: u64,
    name: String,
    enabled: bool,
}

pub fn make_trigger_events(tc: &ParsedToolCall, result_str: &str) -> Vec<TriggerEvent> {
    if result_str.starts_with("error:") {
        return Vec::new();
    }

    match tc.name.as_str() {
        "trigger__upsert" => serde_json::from_str::<TriggerUpsertEventResult>(result_str)
            .map(|result| {
                vec![TriggerEvent::Upserted {
                    id: result.item.id,
                    name: result.item.name,
                    enabled: result.item.enabled,
                }]
            })
            .unwrap_or_default(),
        "trigger__delete" => {
            if !result_str.starts_with("Removed trigger") {
                return Vec::new();
            }

            tc.arguments["id"]
                .as_u64()
                .map(|id| vec![TriggerEvent::Deleted { id }])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::make_trigger_events;
    use crate::events::TriggerEvent;
    use remi_agentloop::prelude::ParsedToolCall;
    use serde_json::json;

    #[test]
    fn upsert_event_emits_from_result_payload() {
        let tc = ParsedToolCall {
            id: "call-1".to_string(),
            name: "trigger__upsert".to_string(),
            arguments: json!({
                "name": "Morning summary",
                "request": "Send a summary",
                "precondition": [{
                    "rule": "cron('0 9 * * *')",
                    "description": "Every day at 09:00"
                }],
                "condition": []
            }),
        };

        let result = serde_json::to_string_pretty(&json!({
            "operation": "created",
            "item": {
                "id": 3,
                "name": "Morning summary",
                "enabled": true
            }
        }))
        .unwrap();

        let events = make_trigger_events(&tc, &result);
        assert_eq!(events.len(), 1);
        match &events[0] {
            TriggerEvent::Upserted { id, name, enabled } => {
                assert_eq!(*id, 3);
                assert_eq!(name, "Morning summary");
                assert!(*enabled);
            }
            other => panic!("expected upsert event, got {other:?}"),
        }
    }
}
