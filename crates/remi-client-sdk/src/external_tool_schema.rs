use prost_types::{Struct as ProstStruct, Value as ProstValue};
use serde_json::{Value as JsonValue, json};

const SUPPRESSED_TOOL_NAMES: &[&str] = &[
    "describe_skill",
    "handoff_to_deep_agent",
    "create_trigger_simple",
    "create_trigger",
    "delete_trigger",
];
const MANAGED_EXTERNAL_TOOL_NAMES: &[&str] = &[
    "test_trigger",
    "ls_tool",
    "cat_tool",
    "create_tool",
    "create_timer_trigger",
    "tree_tool",
    "add_things_tool",
    "edit_path_tool",
    "delete_path_tool",
    "move_path_tool",
    "fetch",
    "retrieve_events",
    "abstract_events",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatAgentMode {
    Ask,
    Manager,
    Refiner,
}

fn normalize_active_agent_name(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "light" => "ask".to_string(),
        "deep" | "auto" | "" => "manager".to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn chat_start_extra_tools(user_state: Option<&JsonValue>) -> Vec<ProstStruct> {
    tool_definitions_for_user_state(user_state)
        .into_iter()
        .map(json_to_prost_struct)
        .collect()
}

pub(crate) fn normalize_resume_state_tool_definitions(mut state: JsonValue) -> JsonValue {
    let Some(state_obj) = state.as_object_mut() else {
        return state;
    };

    let mut definitions = tool_definitions_for_user_state(state_obj.get("user_state"));
    let preserved_extra_defs = state_obj
        .get("tool_definitions")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .filter(|definition| {
            tool_name(definition).is_none_or(|name| {
                !SUPPRESSED_TOOL_NAMES.contains(&name)
                    && !MANAGED_EXTERNAL_TOOL_NAMES.contains(&name)
            })
        })
        .cloned()
        .collect::<Vec<_>>();

    definitions.extend(preserved_extra_defs);
    state_obj.insert(
        "tool_definitions".to_string(),
        JsonValue::Array(dedupe_tool_definitions(definitions)),
    );

    state
}

fn mode_for_user_state(user_state: Option<&JsonValue>) -> ChatAgentMode {
    user_state
        .and_then(|value| {
            value
                .get("remi_handoff")
                .and_then(|handoff| handoff.get("current_agent"))
                .or_else(|| value.get("agent_mode"))
        })
        .and_then(JsonValue::as_str)
        .map(
            |agent_name| match normalize_active_agent_name(agent_name).as_str() {
                "ask" => ChatAgentMode::Ask,
                "refiner" => ChatAgentMode::Refiner,
                _ => ChatAgentMode::Manager,
            },
        )
        .unwrap_or(ChatAgentMode::Manager)
}

fn configured_external_tool_names(user_state: Option<&JsonValue>) -> Option<Vec<String>> {
    user_state
        .and_then(|value| value.get("configured_external_tool_names"))
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|items| !items.is_empty())
}

fn configured_tool_definitions(tool_names: &[String]) -> Vec<JsonValue> {
    dedupe_tool_definitions(
        tool_names
            .iter()
            .filter_map(|name| tool_definition_for_name(name))
            .collect(),
    )
}

fn tool_definition_for_name(name: &str) -> Option<JsonValue> {
    match name {
        "test_trigger" => Some(test_trigger()),
        "ls_tool" => Some(ls_tool()),
        "cat_tool" => Some(cat_tool()),
        "create_tool" => Some(create_tool()),
        "create_timer_trigger" => Some(create_timer_trigger()),
        "tree_tool" => Some(tree_tool()),
        "edit_path_tool" => Some(edit_path_tool()),
        "delete_path_tool" => Some(delete_path_tool()),
        "move_path_tool" => Some(move_path_tool()),
        "fetch" => Some(fetch()),
        "retrieve_events" => Some(retrieve_events()),
        "abstract_events" => Some(abstract_events()),
        "agent_doc_read" => Some(agent_doc_read()),
        "agent_doc_edit" => Some(agent_doc_edit()),
        _ => None,
    }
}

fn tool_definitions_for_user_state(user_state: Option<&JsonValue>) -> Vec<JsonValue> {
    if let Some(tool_names) = configured_external_tool_names(user_state) {
        let resolved = configured_tool_definitions(&tool_names);
        let resolved_names = resolved
            .iter()
            .filter_map(tool_name)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let unresolved_names = tool_names
            .iter()
            .filter(|name| !resolved_names.iter().any(|resolved| resolved == *name))
            .cloned()
            .collect::<Vec<_>>();
        tracing::info!(
            configured_external_tool_names = ?tool_names,
            resolved_external_tool_names = ?resolved_names,
            unresolved_external_tool_names = ?unresolved_names,
            resolved_external_tool_count = resolved.len(),
            "[external_tool_schema] resolved explicit configured tool names"
        );
        return resolved;
    }

    let mode = mode_for_user_state(user_state);
    let definitions = match mode {
        ChatAgentMode::Ask => ask_tool_definitions(),
        ChatAgentMode::Manager => manager_tool_definitions(),
        ChatAgentMode::Refiner => refiner_tool_definitions(),
    };
    let emitted_external_tool_names = definitions
        .iter()
        .filter_map(tool_name)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    tracing::info!(
        agent_mode = ?mode,
        emitted_external_tool_names = ?emitted_external_tool_names,
        emitted_external_tool_count = definitions.len(),
        "[external_tool_schema] resolved fallback tool set from agent mode"
    );
    definitions
}

fn dedupe_tool_definitions(definitions: Vec<JsonValue>) -> Vec<JsonValue> {
    let mut seen = std::collections::HashSet::new();
    let mut deduped = Vec::with_capacity(definitions.len());

    for definition in definitions {
        let Some(name) = tool_name(&definition) else {
            continue;
        };

        if seen.insert(name.to_string()) {
            deduped.push(definition);
        }
    }

    deduped
}

fn tool_name(definition: &JsonValue) -> Option<&str> {
    definition
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(JsonValue::as_str)
}

fn json_to_prost_struct(value: JsonValue) -> ProstStruct {
    let JsonValue::Object(map) = value else {
        return ProstStruct::default();
    };

    let fields = map
        .into_iter()
        .map(|(key, value)| (key, json_to_prost_value(value)))
        .collect();

    ProstStruct { fields }
}

fn json_to_prost_value(value: JsonValue) -> ProstValue {
    use prost_types::value::Kind;

    let kind = match value {
        JsonValue::Null => Kind::NullValue(0),
        JsonValue::Bool(value) => Kind::BoolValue(value),
        JsonValue::Number(value) => Kind::NumberValue(value.as_f64().unwrap_or(0.0)),
        JsonValue::String(value) => Kind::StringValue(value),
        JsonValue::Array(values) => Kind::ListValue(prost_types::ListValue {
            values: values.into_iter().map(json_to_prost_value).collect(),
        }),
        JsonValue::Object(_) => Kind::StructValue(json_to_prost_struct(value)),
    };

    ProstValue { kind: Some(kind) }
}

fn tool(name: &str, description: &str, parameters: JsonValue) -> JsonValue {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    })
}

fn obj(props: JsonValue, required: &[&str]) -> JsonValue {
    json!({
        "type": "object",
        "properties": props,
        "required": required
    })
}

fn str_prop(description: &str) -> JsonValue {
    json!({ "type": "string", "description": description })
}

fn int_prop(description: &str) -> JsonValue {
    json!({ "type": "integer", "description": description })
}

fn nullable_str(description: &str) -> JsonValue {
    json!({ "type": ["string", "null"], "description": description })
}

fn fetch() -> JsonValue {
    tool(
        "fetch",
        "Fetch a URL, local file path, or remi:// URI and return Markdown-first content, metadata, or direct image content.",
        obj(
            json!({
                "uri": str_prop("URL, local file path, file:// URL, or remi:// URI to fetch. Accepts http/https URLs, absolute or relative local paths, file:// URLs, and remi://file/<path>?type=<mime> URIs.")
            }),
            &["uri"],
        ),
    )
}

fn tree_tool() -> JsonValue {
    tool(
        "tree_tool",
        "Render the virtual Remi filesystem as a Unix tree. The root is '/', with /trigger, /action, and /collection subtrees.",
        obj(
            json!({
                "path": nullable_str("Directory path to render. Defaults to '/'. Examples: '/', '/trigger', '/action', '/collection/<uuid>', '/collection/<uuid>/things/<thing_uuid>'.")
            }),
            &[],
        ),
    )
}

fn edit_path_tool() -> JsonValue {
    tool(
        "edit_path_tool",
        "Edit a file node in the virtual Remi filesystem. For metadata files such as name, trigger, action.json, actions.json, status, and rule.json, omit operation and the tool defaults to overwrite. When editing status, use one of: none, in-progress, stalled, done. content.md supports overwrite, append, str_replace, and insert_at_line.",
        obj(
            json!({
                "path": str_prop("Absolute file path to edit, such as '/trigger/<uuid>/action.json', '/collection/<collection_uuid>/actions.json', or '/collection/<collection_uuid>/things/<thing_uuid>/entries.0'."),
                "operation": {
                    "type": ["string", "null"],
                    "description": "Optional edit operation. Omit it for metadata files such as name, trigger, action.json, actions.json, status, and rule.json; they default to 'overwrite'. content.md also supports append, str_replace, and insert_at_line.",
                    "enum": ["overwrite", "append", "str_replace", "insert_at_line", null]
                },
                "value": {
                    "description": "Replacement or inserted value. Use a string for name, trigger, status, and content.md. Use null or an object for /trigger/<uuid>/action.json. Use an action binding object or array for actions.json. Status accepts: none, in-progress, stalled, done. Use an object for rule.json and entries.{idx}."
                },
                "old_str": nullable_str("Required for str_replace on content.md."),
                "new_str": nullable_str("Replacement text for str_replace on content.md."),
                "line_number": int_prop("Required for insert_at_line on content.md. Uses the existing 1-based editor semantics, with 0 meaning prepend.")
            }),
            &["path"],
        ),
    )
}

fn delete_path_tool() -> JsonValue {
    tool(
        "delete_path_tool",
        "Delete an entity directory or entry file from the virtual Remi filesystem. Supports trigger directories, collection directories, thing directories, and entries.{idx}.",
        obj(
            json!({
                "path": str_prop("Absolute path to delete. Examples: '/trigger/<uuid>', '/collection/<uuid>', '/collection/<collection_uuid>/things/<thing_uuid>', '/collection/<collection_uuid>/things/<thing_uuid>/entries.0'.")
            }),
            &["path"],
        ),
    )
}

fn move_path_tool() -> JsonValue {
    tool(
        "move_path_tool",
        "Move a thing directory to another things directory in the virtual Remi filesystem.",
        obj(
            json!({
                "from_path": str_prop("Source thing directory path, such as '/collection/<collection_uuid>/things/<thing_uuid>'."),
                "to_path": str_prop("Destination things directory path, such as '/collection/<collection_uuid>/things' or '/collection/<collection_uuid>/things/<thing_uuid>/things'.")
            }),
            &["from_path", "to_path"],
        ),
    )
}

fn test_trigger() -> JsonValue {
    tool(
        "test_trigger",
        "Validate a trigger JSON configuration using the CEL engine.",
        obj(
            json!({
                "trigger": str_prop("Complete trigger configuration as JSON string.")
            }),
            &["trigger"],
        ),
    )
}

fn retrieve_events() -> JsonValue {
    tool(
        "retrieve_events",
        "Retrieve telemetry events within an explicit time window. If the requested window is empty but the device has recorded events, the result may include available_time_range with the earliest and latest recorded event timestamps.",
        obj(
            json!({
                "start_time": str_prop("ISO-8601 start timestamp."),
                "end_time": str_prop("ISO-8601 end timestamp.")
            }),
            &["start_time", "end_time"],
        ),
    )
}

fn ls_tool() -> JsonValue {
    tool(
        "ls_tool",
        "List a directory in the virtual Remi filesystem using the same tree-style output as tree_tool. Supports '/', '/trigger', '/action', '/collection', and nested things directories.",
        obj(
            json!({
                "path": nullable_str("Directory path to list. Defaults to '/'.")
            }),
            &[],
        ),
    )
}

fn abstract_events() -> JsonValue {
    tool(
        "abstract_events",
        "Create an hourly abstract of telemetry events with totals and top event types.",
        obj(
            json!({
                "top_n": int_prop("Number of top event types per hour to include (default 3).")
            }),
            &[],
        ),
    )
}

fn cat_tool() -> JsonValue {
    tool(
        "cat_tool",
        "Read a virtual filesystem file. For image entries, cat_tool returns the image directly instead of JSON text. Valid file nodes include trigger name/rule.json/action.json, action name/metadata.json/input.schema.json/output.schema.json/script.js/latest-invocation.json, and collection or thing name/trigger/actions.json/status/content.md/entries.{idx}/entries.{idx}.data.json/entries.{idx}.schema.json.",
        obj(
            json!({
                "path": str_prop("Absolute file path to read, such as '/trigger/<uuid>/action.json', '/action/<action_uuid>/metadata.json', '/collection/<collection_uuid>/actions.json', '/collection/<collection_uuid>/things/<thing_uuid>/entries.1', or '/collection/<collection_uuid>/things/<thing_uuid>/entries.1.data.json'.")
            }),
            &["path"],
        ),
    )
}

fn create_tool() -> JsonValue {
    tool(
        "create_tool",
        "Create a new collection, thing, image entry, json_object entry, or action binding from a parent path. The tool generates a new UUID automatically when creating collections, things, or entries, and returns the created UUID and path.",
        obj(
            json!({
                "parent_path": str_prop("Parent path. Use '/' or '/collection' for collections. Use '/collection/<collection_uuid>/things' or '/collection/<collection_uuid>/things/<thing_uuid>/things' for things. Use '/collection/<collection_uuid>/things/<thing_uuid>' for image, json_object, or thing-level action_binding. Use '/collection/<collection_uuid>' for collection-level action_binding. Use '/trigger/<trigger_uuid>' for trigger action_binding."),
                "type_name": {
                    "type": "string",
                    "description": "Entity type to create.",
                    "enum": ["collection", "thing", "image", "json_object", "action_binding"]
                },
                "action_uuid": nullable_str("Required for type_name='action_binding'. Names the action to bind, such as 'builtin.echo_json'."),
                "title": nullable_str("Optional initial title. Defaults to 'New Collection' or 'New Thing'. For image or json_object entries this becomes the entry title. For collection/thing action_binding this becomes label_override."),
                "content": nullable_str("Optional initial markdown content for things. For json_object entries this may be a JSON object string used as initial data. For action_binding this may be a JSON string used as args_json."),
                "source_uri": nullable_str("Required for type_name='image'. Must be a remi:// URI, typically one of the current chat input image attachments.")
            }),
            &["parent_path", "type_name"],
        ),
    )
}

fn create_timer_trigger() -> JsonValue {
    tool(
        "create_timer_trigger",
        "Create or update a simple timer-based or cron-based trigger and bind it to a thing or collection in one step. Provide exactly one of cron or timer_condition. This is the fast path for simple reminders; for more complex triggers, call trigger_subagent.",
        obj(
            json!({
                "binding_uuid": str_prop("UUID of the thing or collection that should receive the trigger binding."),
                "binding_type": {
                    "type": "string",
                    "description": "Binding target type.",
                    "enum": ["thing", "collection"]
                },
                "name": str_prop("Human-readable trigger name."),
                "cron": nullable_str("POSIX 5-field cron expression such as '0 9 * * 1-5'. Use this for recurring schedules. Provide cron or timer_condition, not both."),
                "timer_condition": nullable_str("One-shot timer value. Accepts either a raw timer(...) expression or a bare value such as '2026-04-05T09:00:00+08:00' or '30min'. Provide timer_condition or cron, not both."),
                "trigger_uuid": nullable_str("Optional existing trigger UUID to update instead of creating a fresh one."),
                "user_request": nullable_str("Optional original user request for audit/debug context.")
            }),
            &["binding_uuid", "binding_type", "name"],
        ),
    )
}

fn ask_tool_definitions() -> Vec<JsonValue> {
    vec![
        test_trigger(),
        ls_tool(),
        cat_tool(),
        tree_tool(),
        fetch(),
        retrieve_events(),
        abstract_events(),
    ]
}

fn manager_tool_definitions() -> Vec<JsonValue> {
    vec![
        test_trigger(),
        ls_tool(),
        cat_tool(),
        create_tool(),
        create_timer_trigger(),
        tree_tool(),
        edit_path_tool(),
        delete_path_tool(),
        move_path_tool(),
        fetch(),
        retrieve_events(),
        abstract_events(),
    ]
}

fn refiner_tool_definitions() -> Vec<JsonValue> {
    vec![agent_doc_read(), agent_doc_edit()]
}

fn agent_doc_read() -> JsonValue {
    tool(
        "agent_doc_read",
        "Read the current markdown for an agent live draft or saved version under review.",
        obj(
            json!({
                "agent_id": str_prop("Target agent definition id, such as 'manager' or 'lab_agent_refiner'."),
                "version_id": nullable_str("Optional saved version id. Omit or pass null to read the live draft.")
            }),
            &["agent_id"],
        ),
    )
}

fn agent_doc_edit() -> JsonValue {
    tool(
        "agent_doc_edit",
        "Replace an agent live draft or saved version with a validated full markdown document.",
        obj(
            json!({
                "agent_id": str_prop("Target agent definition id for the draft or version being edited."),
                "version_id": nullable_str("Optional saved version id. Omit or pass null to edit the live draft."),
                "raw_markdown": str_prop("Full replacement markdown, including YAML frontmatter and prompt body.")
            }),
            &["agent_id", "raw_markdown"],
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_names(definitions: &[JsonValue]) -> Vec<String> {
        definitions
            .iter()
            .filter_map(tool_name)
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn ask_mode_exposes_only_read_only_tools() {
        let names = tool_names(&ask_tool_definitions());

        assert!(names.iter().any(|name| name == "ls_tool"));
        assert!(names.iter().any(|name| name == "cat_tool"));
        assert!(names.iter().any(|name| name == "retrieve_events"));
        assert!(names.iter().any(|name| name == "abstract_events"));
        assert!(!names.iter().any(|name| name == "create_tool"));
        assert!(!names.iter().any(|name| name == "create_trigger"));
        assert!(!names.iter().any(|name| name == "create_trigger_simple"));
        assert!(!names.iter().any(|name| name == "delete_trigger"));
    }

    #[test]
    fn manager_mode_exposes_full_tool_set() {
        let names = tool_names(&manager_tool_definitions());

        assert!(names.iter().any(|name| name == "ls_tool"));
        assert!(names.iter().any(|name| name == "cat_tool"));
        assert!(names.iter().any(|name| name == "retrieve_events"));
        assert!(names.iter().any(|name| name == "create_tool"));
        assert!(names.iter().any(|name| name == "create_timer_trigger"));
        assert!(!names.iter().any(|name| name == "create_trigger"));
        assert!(!names.iter().any(|name| name == "delete_trigger"));
        assert!(!names.iter().any(|name| name == "add_things_tool"));
        assert!(!names.iter().any(|name| name == "create_trigger_simple"));
    }

    #[test]
    fn refiner_mode_exposes_agent_doc_tools_only() {
        let names = tool_names(&tool_definitions_for_user_state(Some(&json!({
            "agent_mode": "refiner"
        }))));

        assert!(names.iter().any(|name| name == "agent_doc_read"));
        assert!(names.iter().any(|name| name == "agent_doc_edit"));
        assert!(!names.iter().any(|name| name == "create_tool"));
        assert!(!names.iter().any(|name| name == "ls_tool"));
    }

    #[test]
    fn configured_external_tool_names_override_mode_defaults() {
        let names = tool_names(&tool_definitions_for_user_state(Some(&json!({
            "agent_mode": "manager",
            "configured_external_tool_names": ["cat_tool", "fetch", "preview_card"]
        }))));

        assert!(names.iter().any(|name| name == "cat_tool"));
        assert!(names.iter().any(|name| name == "fetch"));
        assert!(!names.iter().any(|name| name == "preview_card"));
        assert!(!names.iter().any(|name| name == "tree_tool"));
    }

    #[test]
    fn namespaced_handoff_state_selects_manager_tool_set() {
        let names = tool_names(&tool_definitions_for_user_state(Some(&json!({
            "remi_handoff": { "current_agent": "manager" }
        }))));

        assert!(names.iter().any(|name| name == "ls_tool"));
        assert!(names.iter().any(|name| name == "retrieve_events"));
        assert!(!names.iter().any(|name| name == "create_trigger"));
    }

    #[test]
    fn legacy_agent_mode_still_selects_manager_tool_set() {
        let names = tool_names(&tool_definitions_for_user_state(Some(&json!({
            "agent_mode": "deep"
        }))));

        assert!(names.iter().any(|name| name == "ls_tool"));
        assert!(names.iter().any(|name| name == "retrieve_events"));
        assert!(!names.iter().any(|name| name == "create_trigger"));
    }

    #[test]
    fn normalize_resume_state_swaps_managed_tool_set_by_mode() {
        let mut state = json!({
            "user_state": { "agent_mode": "manager" },
            "tool_definitions": ask_tool_definitions(),
        });
        state["tool_definitions"]
            .as_array_mut()
            .expect("tool_definitions should be an array")
            .push(json!({
                "type": "function",
                "function": {
                    "name": "custom_external_tool",
                    "description": "custom",
                    "parameters": {"type": "object", "properties": {}, "required": []}
                }
            }));

        let normalized = normalize_resume_state_tool_definitions(state);
        let names = tool_names(
            normalized["tool_definitions"]
                .as_array()
                .expect("normalized tool_definitions should be an array"),
        );

        assert!(names.iter().any(|name| name == "retrieve_events"));
        assert!(!names.iter().any(|name| name == "create_trigger"));
        assert!(names.iter().any(|name| name == "custom_external_tool"));
        assert!(!names.iter().any(|name| name == "create_trigger_simple"));
    }

    #[test]
    fn normalize_resume_state_uses_namespaced_handoff_mode() {
        let state = json!({
            "user_state": {
                "remi_handoff": { "current_agent": "manager" }
            },
            "tool_definitions": ask_tool_definitions(),
        });

        let normalized = normalize_resume_state_tool_definitions(state);
        let names = tool_names(
            normalized["tool_definitions"]
                .as_array()
                .expect("normalized tool_definitions should be an array"),
        );

        assert!(names.iter().any(|name| name == "ls_tool"));
        assert!(names.iter().any(|name| name == "retrieve_events"));
        assert!(!names.iter().any(|name| name == "create_trigger"));
    }

    #[test]
    fn light_alias_selects_ask_tool_set() {
        let names = tool_names(&tool_definitions_for_user_state(Some(&json!({
            "agent_mode": "light"
        }))));

        assert!(names.iter().any(|name| name == "retrieve_events"));
        assert!(!names.iter().any(|name| name == "create_tool"));
        assert!(!names.iter().any(|name| name == "create_trigger"));
    }

    #[test]
    fn edit_path_tool_only_requires_path_and_keeps_operation_optional() {
        let schema = edit_path_tool();
        let parameters = &schema["function"]["parameters"];
        let required = parameters["required"]
            .as_array()
            .expect("required should be an array");

        assert_eq!(required, &vec![json!("path")]);
        assert!(
            parameters["properties"]["operation"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Omit it for metadata files")
        );
    }
}
