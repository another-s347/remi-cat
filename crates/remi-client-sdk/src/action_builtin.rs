use serde_json::json;

use crate::types::ActionDefinition;

pub(crate) fn builtin_actions() -> Vec<ActionDefinition> {
    vec![
        ActionDefinition {
            action_uuid: "builtin.echo_json".to_string(),
            name: "echo_json".to_string(),
            title: "Echo JSON".to_string(),
            description: "Return the resolved action payload for verification and smoke testing."
                .to_string(),
            version: "v1".to_string(),
            category: "utility".to_string(),
            enabled: true,
            metadata_json: json!({
                "builtin": true,
                "supports_trigger": true,
                "supports_manual": true,
                "summary": "Returns the action input, source, and context as structured JSON."
            }),
            script_source: r#"
console.log(`running ${action.uuid}`);
return {
    ok: true,
    action,
    source,
    args,
    context,
};
"#
            .trim()
            .to_string(),
            input_schema_json: json!({
                "type": "object",
                "properties": {
                    "args": { "type": ["object", "array", "string", "number", "boolean", "null"] },
                    "source": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string" },
                            "entity_type": { "type": ["string", "null"] },
                            "entity_uuid": { "type": ["string", "null"] }
                        },
                        "required": ["kind"]
                    },
                    "context": { "type": "object" }
                },
                "required": ["args", "source", "context"]
            }),
            output_schema_json: Some(json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "action": { "type": "object" },
                    "source": { "type": "object" },
                    "args": {},
                    "context": { "type": "object" }
                },
                "required": ["ok", "action", "source", "args", "context"]
            })),
        },
        ActionDefinition {
            action_uuid: "builtin.trigger_notification".to_string(),
            name: "trigger_notification".to_string(),
            title: "Trigger Notification".to_string(),
            description: "Persist the default local notification for a fired trigger.".to_string(),
            version: "v1".to_string(),
            category: "notification".to_string(),
            enabled: true,
            metadata_json: json!({
                "builtin": true,
                "supports_trigger": true,
                "supports_manual": false,
                "implicit_default": true,
                "summary": "Stores the default trigger notification with the same title/body/category behavior as legacy trigger notifications."
            }),
            script_source: r#"
const payload = {
    title: args.title ?? action.title,
    body: args.body ?? "",
    category: args.category ?? source.entity_uuid ?? `action:${action.uuid}`,
    source: args.source ?? "trigger",
};
console.log(`sending trigger notification for ${payload.category}`);
return notify.send(payload);
"#
            .trim()
            .to_string(),
            input_schema_json: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "category": { "type": "string" },
                    "source": { "type": "string", "enum": ["trigger", "system", "chat", "push"] }
                },
                "required": ["title", "body", "category", "source"]
            }),
            output_schema_json: Some(json!({
                "type": "object",
                "properties": {
                    "notification_id": { "type": "integer" },
                    "source": { "type": "string" },
                    "category": { "type": "string" },
                    "title": { "type": "string" },
                    "body": { "type": "string" }
                },
                "required": ["notification_id", "source", "category", "title", "body"]
            })),
        },
    ]
}
