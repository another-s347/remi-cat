use serde_json::{Value as JsonValue, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

use crate::chat_types::{
    PendingInterrupt, PendingToolCall, PendingToolExecutionState, RichHandlerResult,
    ToolExecutionOutcome,
};
use crate::external_tool_handler::ExternalToolHandler;

#[derive(Debug, Clone)]
pub struct ExternalToolCallRequest {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: JsonValue,
}

#[derive(Debug, Clone, Default)]
pub struct ExternalToolExecutionPlan {
    pub resolved_results: Vec<ToolExecutionOutcome>,
    pub pending_calls: Vec<PendingToolCall>,
    pub first_pending_interrupt: Option<PendingInterrupt>,
    pub things_changed: bool,
    pub trigger_scheduler_sync_needed: bool,
}

#[derive(Clone, Default)]
pub struct ExternalToolExecutor {
    handlers: HashMap<String, Arc<dyn ExternalToolHandler>>,
}

impl ExternalToolExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<H: ExternalToolHandler + 'static>(
        &mut self,
        tool_kind: impl Into<String>,
        handler: H,
    ) {
        self.handlers.insert(tool_kind.into(), Arc::new(handler));
    }

    pub fn with_handler<H: ExternalToolHandler + 'static>(
        mut self,
        tool_kind: impl Into<String>,
        handler: H,
    ) -> Self {
        self.register(tool_kind, handler);
        self
    }

    pub fn has_handler(&self, tool_kind: &str) -> bool {
        self.handlers.contains_key(tool_kind)
    }

    pub async fn resolve_calls(
        &self,
        calls: impl IntoIterator<Item = ExternalToolCallRequest>,
    ) -> ExternalToolExecutionPlan {
        let mut plan = ExternalToolExecutionPlan::default();

        for tool_call in calls {
            let display_data =
                tool_call_display_payload(&tool_call.tool_name, &tool_call.arguments);
            let tool_kind = tool_call_kind(&display_data).map(ToString::to_string);
            let pending_call = PendingToolCall {
                tool_call_id: tool_call.tool_call_id.clone(),
                tool_name: tool_call.tool_name.clone(),
                arguments: tool_call.arguments.clone(),
                display_data: display_data.clone(),
            };

            let Some(tool_kind) = tool_kind else {
                plan.pending_calls.push(pending_call);
                continue;
            };

            let Some(handler) = self.handlers.get(&tool_kind).cloned() else {
                plan.pending_calls.push(pending_call);
                continue;
            };

            tracing::info!(
                tool_call_id = %tool_call.tool_call_id,
                tool_kind = %tool_kind,
                tool_name = %tool_call.tool_name,
                uri = handler_uri_hint(&display_data),
                "[ExternalToolExecutor] Executing local tool handler"
            );

            let started_at = Instant::now();
            let resume_value = match handler
                .handle_rich(&tool_call.tool_call_id, &display_data)
                .await
            {
                Ok(result) => result,
                Err(error) => RichHandlerResult::Json(json!({
                    "error": error,
                    "tool_kind": tool_kind,
                })),
            };
            tracing::info!(
                tool_call_id = %tool_call.tool_call_id,
                tool_kind = %tool_kind,
                tool_name = %tool_call.tool_name,
                uri = handler_uri_hint(&display_data),
                elapsed_ms = started_at.elapsed().as_millis(),
                "[ExternalToolExecutor] Local tool handler finished"
            );

            let outcome = raw_resume_value_to_outcome(
                &pending_call,
                resume_value,
                Some(started_at.elapsed().as_millis() as u64),
            );
            if tool_kind == "trigger_rule_published" && outcome.error.is_none() {
                plan.trigger_scheduler_sync_needed = true;
            }
            plan.things_changed = true;
            plan.resolved_results.push(outcome);
        }

        plan.first_pending_interrupt = plan.pending_calls.first().map(|call| PendingInterrupt {
            interrupt_id: call.tool_call_id.clone(),
            interrupt_type: {
                tool_call_kind(&call.display_data)
                    .filter(|value| !value.trim().is_empty())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| call.tool_name.clone())
            },
            display_data: call.display_data.clone(),
        });

        plan
    }
}

pub fn manual_resume_outcomes(
    pending: &PendingToolExecutionState,
    resume_value: JsonValue,
) -> Result<Vec<ToolExecutionOutcome>, String> {
    if pending.pending_calls.is_empty() {
        return Ok(Vec::new());
    }

    let mapping = resume_value.as_object().cloned();
    let mut outcomes = Vec::with_capacity(pending.pending_calls.len());

    for call in &pending.pending_calls {
        let raw = mapping
            .as_ref()
            .and_then(|map| map.get(&call.tool_call_id).cloned())
            .or_else(|| {
                if pending.pending_calls.len() == 1 {
                    Some(resume_value.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| format!("Missing resume value for tool call {}", call.tool_call_id))?;

        outcomes.push(raw_resume_value_to_outcome(
            call,
            RichHandlerResult::Json(raw),
            None,
        ));
    }

    Ok(outcomes)
}

fn tool_call_display_payload(tool_name: &str, arguments: &JsonValue) -> JsonValue {
    match tool_name {
        "ls_tool" => merge_tool_payload("virtual_fs_ls_request", arguments),
        "cat_tool" => merge_tool_payload("virtual_fs_cat_request", arguments),
        "create_tool" => merge_tool_payload("virtual_fs_create_request", arguments),
        "tree_tool" => merge_tool_payload("virtual_fs_tree_request", arguments),
        "preview_card" => merge_tool_payload("rendera_preview_card", arguments),
        "read_path_tool" => merge_tool_payload("virtual_fs_cat_request", arguments),
        "add_things_tool" => merge_tool_payload("things_thing_added", arguments),
        "edit_path_tool" => merge_tool_payload("virtual_fs_edit_request", arguments),
        "delete_path_tool" => merge_tool_payload("virtual_fs_delete_request", arguments),
        "move_path_tool" => merge_tool_payload("virtual_fs_move_request", arguments),
        "create_trigger" => build_trigger_publish_payload(arguments),
        "create_timer_trigger" | "create_trigger_simple" => {
            build_timer_trigger_publish_payload(arguments)
        }
        "delete_trigger" => merge_tool_payload("trigger_rule_deleted", arguments),
        "test_trigger" => json!({
            "type": "trigger_test_request",
            "trigger_json": arguments.get("trigger").cloned().unwrap_or_else(|| JsonValue::String(String::new())),
            "start_iso": JsonValue::Null,
            "end_iso": JsonValue::Null,
            "manual": false,
        }),
        "retrieve_events" => json!({
            "type": "events_retrieve_request",
            "start_time": arguments.get("start_time").cloned().unwrap_or_else(|| JsonValue::String(String::new())),
            "end_time": arguments.get("end_time").cloned().unwrap_or_else(|| JsonValue::String(String::new())),
        }),
        "abstract_events" => json!({
            "type": "events_abstract_request",
            "top_n": arguments.get("top_n").cloned().unwrap_or_else(|| json!(3)),
        }),
        "fetch" | "resolve_uri" => json!({
            "type": "fetch_request",
            "uri": arguments.get("uri").cloned().unwrap_or_else(|| JsonValue::String(String::new())),
        }),
        "agent_doc_read" => json!({
            "type": "agent_doc_read_request",
            "agent_id": arguments.get("agent_id").cloned().unwrap_or_else(|| JsonValue::String(String::new())),
            "version_id": arguments.get("version_id").cloned().unwrap_or(JsonValue::Null),
        }),
        "agent_doc_edit" => json!({
            "type": "agent_doc_edit_request",
            "agent_id": arguments.get("agent_id").cloned().unwrap_or_else(|| JsonValue::String(String::new())),
            "version_id": arguments.get("version_id").cloned().unwrap_or(JsonValue::Null),
            "raw_markdown": arguments.get("raw_markdown").cloned().unwrap_or_else(|| JsonValue::String(String::new())),
        }),
        _ => json!({
            "type": "external_tool_call",
            "tool_name": tool_name,
            "arguments": arguments,
        }),
    }
}

fn build_trigger_publish_payload(arguments: &JsonValue) -> JsonValue {
    let name = trigger_name_from_arguments(arguments);
    let rule_config_json = trigger_rule_config_from_arguments(arguments, &name);

    json!({
        "type": "trigger_rule_published",
        "trigger_uuid": trigger_uuid_from_arguments(arguments),
        "name": name,
        "rule_config_json": rule_config_json,
        "user_request": string_arg(arguments, &["user_request"]),
        "event_analysis": string_arg(arguments, &["event_analysis"]),
        "bind_uuid": string_arg(arguments, &["binding_uuid", "bind_uuid"]).unwrap_or_default(),
        "bind_type": string_arg(arguments, &["binding_type", "bind_type"]).unwrap_or_else(|| "thing".to_string()),
        "version": if arguments.get("trigger_uuid").is_some() { json!(2) } else { json!(1) }
    })
}

fn build_timer_trigger_publish_payload(arguments: &JsonValue) -> JsonValue {
    let name = trigger_name_from_arguments(arguments);
    let cron = string_arg(arguments, &["cron"]);
    let timer_condition = string_arg(arguments, &["timer_condition"]);
    let legacy_condition = string_arg(arguments, &["condition"]);

    let precondition_rule = match (timer_condition.as_deref(), cron.as_deref()) {
        (Some(timer), _) if !timer.trim().is_empty() => normalize_timer_rule(timer),
        (_, Some(cron_expr)) if !cron_expr.trim().is_empty() => normalize_cron_rule(cron_expr),
        _ => String::new(),
    };

    let precondition = if precondition_rule.is_empty() {
        Vec::<JsonValue>::new()
    } else {
        vec![json!({
            "rule": precondition_rule,
            "description": if timer_condition.as_deref().is_some() { "timer" } else { "cron" },
        })]
    };

    let condition = legacy_condition
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            vec![json!({
                "rule": value,
                "description": "condition",
            })]
        })
        .unwrap_or_default();

    json!({
        "type": "trigger_rule_published",
        "trigger_uuid": trigger_uuid_from_arguments(arguments),
        "name": name,
        "rule_config_json": {
            "name": name,
            "precondition": precondition,
            "condition": condition,
        },
        "user_request": string_arg(arguments, &["user_request"]),
        "bind_uuid": string_arg(arguments, &["binding_uuid", "bind_uuid"]).unwrap_or_default(),
        "bind_type": string_arg(arguments, &["binding_type", "bind_type"]).unwrap_or_else(|| "thing".to_string()),
        "version": if arguments.get("trigger_uuid").is_some() { json!(2) } else { json!(1) }
    })
}

fn string_arg(arguments: &JsonValue, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| arguments.get(*key).and_then(JsonValue::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn trigger_uuid_from_arguments(arguments: &JsonValue) -> String {
    string_arg(arguments, &["trigger_uuid"]).unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn trigger_name_from_arguments(arguments: &JsonValue) -> String {
    string_arg(arguments, &["name"])
        .or_else(|| {
            parse_rule_json(arguments)
                .get("name")
                .and_then(JsonValue::as_str)
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| "New Trigger".to_string())
}

fn trigger_rule_config_from_arguments(arguments: &JsonValue, name: &str) -> JsonValue {
    let mut rule = parse_rule_json(arguments);
    if let Some(object) = rule.as_object_mut() {
        object
            .entry("name".to_string())
            .or_insert_with(|| JsonValue::String(name.to_string()));
    }
    rule
}

fn parse_rule_json(arguments: &JsonValue) -> JsonValue {
    let raw = arguments.get("rule").or_else(|| arguments.get("trigger"));
    match raw {
        Some(JsonValue::String(value)) => serde_json::from_str(value).unwrap_or_else(|_| json!({})),
        Some(other) => other.clone(),
        None => json!({}),
    }
}

fn normalize_cron_rule(cron: &str) -> String {
    let trimmed = cron.trim();
    if trimmed.starts_with("cron(") {
        trimmed.to_string()
    } else {
        format!("cron('{trimmed}')")
    }
}

fn normalize_timer_rule(timer: &str) -> String {
    let trimmed = timer.trim();
    if trimmed.starts_with("timer(") {
        trimmed.to_string()
    } else {
        format!("timer('{trimmed}')")
    }
}

fn tool_call_kind(payload: &JsonValue) -> Option<&str> {
    payload
        .get("type")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn merge_tool_payload(interrupt_type: &str, arguments: &JsonValue) -> JsonValue {
    let mut payload = serde_json::Map::new();
    payload.insert(
        "type".to_string(),
        JsonValue::String(interrupt_type.to_string()),
    );

    if let Some(object) = arguments.as_object() {
        let mut object = object.clone();
        if interrupt_type == "virtual_fs_create_request" {
            if !object.contains_key("type_name") {
                if let Some(kind) = object.remove("type") {
                    object.insert("type_name".to_string(), kind);
                }
            } else {
                object.remove("type");
            }
        }
        payload.extend(object);
    } else {
        payload.insert("arguments".to_string(), arguments.clone());
    }

    JsonValue::Object(payload)
}

fn raw_resume_value_to_outcome(
    call: &PendingToolCall,
    raw: RichHandlerResult,
    duration_ms: Option<u64>,
) -> ToolExecutionOutcome {
    match raw {
        RichHandlerResult::Image(img) => ToolExecutionOutcome {
            tool_call_id: call.tool_call_id.clone(),
            tool_name: call.tool_name.clone(),
            result: None,
            result_parts: Some(vec![img]),
            error: None,
            duration_ms,
        },
        RichHandlerResult::Json(json_val) => {
            let error = json_val
                .get("error")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string())
                .or_else(|| json_val.get("error").map(|value| value.to_string()));

            match error {
                Some(error) => ToolExecutionOutcome {
                    tool_call_id: call.tool_call_id.clone(),
                    tool_name: call.tool_name.clone(),
                    result: None,
                    result_parts: None,
                    error: Some(error),
                    duration_ms,
                },
                None => ToolExecutionOutcome {
                    tool_call_id: call.tool_call_id.clone(),
                    tool_name: call.tool_name.clone(),
                    result: Some(match json_val {
                        JsonValue::String(text) => text,
                        other => serde_json::to_string(&other).unwrap_or_default(),
                    }),
                    result_parts: None,
                    error: None,
                    duration_ms,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::{Value as JsonValue, json};

    use super::{
        ExternalToolCallRequest, ExternalToolExecutor, manual_resume_outcomes,
        tool_call_display_payload,
    };
    use crate::chat_types::{PendingToolCall, PendingToolExecutionState};
    use crate::external_tool_handler::ExternalToolHandler;

    struct EchoHandler;

    #[async_trait]
    impl ExternalToolHandler for EchoHandler {
        async fn handle(
            &self,
            _interrupt_id: &str,
            payload: &JsonValue,
        ) -> Result<JsonValue, String> {
            Ok(payload.clone())
        }
    }

    #[tokio::test]
    async fn resolve_calls_auto_resumes_registered_tools() {
        let mut executor = ExternalToolExecutor::new();
        executor.register("fetch_request", EchoHandler);

        let plan = executor
            .resolve_calls([ExternalToolCallRequest {
                tool_call_id: "fetch:0".to_string(),
                tool_name: "fetch".to_string(),
                arguments: json!({ "uri": "https://example.com" }),
            }])
            .await;

        assert!(plan.pending_calls.is_empty());
        assert_eq!(plan.resolved_results.len(), 1);
        assert_eq!(
            plan.resolved_results[0].result.as_deref(),
            Some("{\"type\":\"fetch_request\",\"uri\":\"https://example.com\"}")
        );
    }

    #[test]
    fn resolve_uri_payload_is_aliased_to_fetch_handler_shape() {
        let payload =
            tool_call_display_payload("resolve_uri", &json!({ "uri": "https://example.com" }));

        assert_eq!(
            payload.get("type").and_then(JsonValue::as_str),
            Some("fetch_request")
        );
        assert_eq!(
            payload.get("uri").and_then(JsonValue::as_str),
            Some("https://example.com")
        );
    }

    #[test]
    fn manual_resume_outcomes_support_single_pending_call() {
        let pending = PendingToolExecutionState {
            state: json!({ "turn": 1 }),
            completed_results: Vec::new(),
            resolved_results: Vec::new(),
            pending_calls: vec![PendingToolCall {
                tool_call_id: "tool:0".to_string(),
                tool_name: "resolve_uri".to_string(),
                arguments: json!({ "uri": "https://example.com" }),
                display_data: json!({ "type": "resolve_uri" }),
            }],
        };

        let outcomes = manual_resume_outcomes(&pending, json!({ "title": "Example" }))
            .expect("single pending call should accept direct value");

        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].result.as_deref(),
            Some("{\"title\":\"Example\"}")
        );
    }

    #[test]
    fn read_path_tool_payload_is_aliased_to_cat_handler_shape() {
        let payload = tool_call_display_payload(
            "read_path_tool",
            &json!({ "path": "/collection/c1/things/t1/content.md" }),
        );

        assert_eq!(
            payload.get("type").and_then(JsonValue::as_str),
            Some("virtual_fs_cat_request")
        );
        assert_eq!(
            payload.get("path").and_then(JsonValue::as_str),
            Some("/collection/c1/things/t1/content.md")
        );
        assert!(payload.get("arguments").is_none());
    }

    #[test]
    fn cat_tool_payload_matches_registered_handler_shape() {
        let payload = tool_call_display_payload(
            "cat_tool",
            &json!({ "path": "/collection/c1/things/t1/entries.1" }),
        );

        assert_eq!(
            payload.get("type").and_then(JsonValue::as_str),
            Some("virtual_fs_cat_request")
        );
        assert_eq!(
            payload.get("path").and_then(JsonValue::as_str),
            Some("/collection/c1/things/t1/entries.1")
        );
        assert!(payload.get("arguments").is_none());
    }

    #[test]
    fn create_tool_payload_matches_registered_handler_shape() {
        let payload = tool_call_display_payload(
            "create_tool",
            &json!({ "parent_path": "/collection/c1/things", "type_name": "thing" }),
        );

        assert_eq!(
            payload.get("type").and_then(JsonValue::as_str),
            Some("virtual_fs_create_request")
        );
        assert_eq!(
            payload.get("parent_path").and_then(JsonValue::as_str),
            Some("/collection/c1/things")
        );
        assert_eq!(
            payload.get("type_name").and_then(JsonValue::as_str),
            Some("thing")
        );
    }

    #[test]
    fn create_tool_payload_drops_legacy_type_when_type_name_is_present() {
        let payload = tool_call_display_payload(
            "create_tool",
            &json!({
                "parent_path": "/collection",
                "type_name": "collection",
                "type": "collection"
            }),
        );

        assert_eq!(
            payload.get("type").and_then(JsonValue::as_str),
            Some("virtual_fs_create_request")
        );
        assert_eq!(
            payload.get("type_name").and_then(JsonValue::as_str),
            Some("collection")
        );
        assert!(payload.get("type").and_then(JsonValue::as_str) != Some("collection"));
    }

    #[test]
    fn create_tool_payload_renames_legacy_type_argument() {
        let payload = tool_call_display_payload(
            "create_tool",
            &json!({ "parent_path": "/collection/c1/things", "type": "thing" }),
        );

        assert_eq!(
            payload.get("type").and_then(JsonValue::as_str),
            Some("virtual_fs_create_request")
        );
        assert_eq!(
            payload.get("type_name").and_then(JsonValue::as_str),
            Some("thing")
        );
    }

    #[test]
    fn create_trigger_payload_matches_registered_handler_shape() {
        let payload = tool_call_display_payload(
            "create_trigger",
            &json!({
                "binding_uuid": "thing-1",
                "binding_type": "thing",
                "name": "Morning reminder",
                "rule": {
                    "precondition": [{"rule": "cron('0 9 * * *')", "description": "daily"}],
                    "condition": []
                }
            }),
        );

        assert_eq!(
            payload.get("type").and_then(JsonValue::as_str),
            Some("trigger_rule_published")
        );
        assert_eq!(
            payload.get("bind_uuid").and_then(JsonValue::as_str),
            Some("thing-1")
        );
        assert_eq!(
            payload.get("bind_type").and_then(JsonValue::as_str),
            Some("thing")
        );
        assert_eq!(
            payload.get("name").and_then(JsonValue::as_str),
            Some("Morning reminder")
        );
        assert_eq!(
            payload
                .get("rule_config_json")
                .and_then(|value| value.get("precondition"))
                .and_then(JsonValue::as_array)
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn create_timer_trigger_payload_builds_timer_rule() {
        let payload = tool_call_display_payload(
            "create_timer_trigger",
            &json!({
                "binding_uuid": "collection-1",
                "binding_type": "collection",
                "name": "One-shot reminder",
                "timer_condition": "2026-04-05T09:00:00+08:00"
            }),
        );

        assert_eq!(
            payload.get("type").and_then(JsonValue::as_str),
            Some("trigger_rule_published")
        );
        assert_eq!(
            payload.get("bind_uuid").and_then(JsonValue::as_str),
            Some("collection-1")
        );
        assert_eq!(
            payload
                .get("rule_config_json")
                .and_then(|value| value.get("precondition"))
                .and_then(|value| value.get(0))
                .and_then(|value| value.get("rule"))
                .and_then(JsonValue::as_str),
            Some("timer('2026-04-05T09:00:00+08:00')")
        );
    }

    #[tokio::test]
    async fn resolve_calls_auto_resumes_read_path_via_cat_handler() {
        let mut executor = ExternalToolExecutor::new();
        executor.register("virtual_fs_cat_request", EchoHandler);

        let plan = executor
            .resolve_calls([ExternalToolCallRequest {
                tool_call_id: "read_path_tool:0".to_string(),
                tool_name: "read_path_tool".to_string(),
                arguments: json!({ "path": "/trigger/t1/rule.json" }),
            }])
            .await;

        assert!(plan.pending_calls.is_empty());
        assert_eq!(plan.resolved_results.len(), 1);
        assert_eq!(
            plan.resolved_results[0].result.as_deref(),
            Some("{\"path\":\"/trigger/t1/rule.json\",\"type\":\"virtual_fs_cat_request\"}")
        );
    }

    #[tokio::test]
    async fn resolve_calls_auto_resumes_ls_when_handler_registered() {
        let mut executor = ExternalToolExecutor::new();
        executor.register("virtual_fs_ls_request", EchoHandler);

        let plan = executor
            .resolve_calls([ExternalToolCallRequest {
                tool_call_id: "ls_tool:0".to_string(),
                tool_name: "ls_tool".to_string(),
                arguments: json!({ "path": "/collection/c1" }),
            }])
            .await;

        assert!(plan.pending_calls.is_empty());
        assert_eq!(plan.resolved_results.len(), 1);
        assert_eq!(
            plan.resolved_results[0].result.as_deref(),
            Some("{\"path\":\"/collection/c1\",\"type\":\"virtual_fs_ls_request\"}")
        );
    }
}

fn handler_uri_hint(payload: &JsonValue) -> &str {
    payload
        .get("uri")
        .and_then(JsonValue::as_str)
        .or_else(|| {
            payload
                .get("arguments")
                .and_then(|value| value.get("uri"))
                .and_then(JsonValue::as_str)
        })
        .unwrap_or("")
}
