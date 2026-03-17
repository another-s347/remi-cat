use std::sync::Arc;

use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{
    AgentError, Tool, ToolContext, ToolDefinitionContext, ToolOutput, ToolResult,
};
use remi_agentloop::types::ResumePayload;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::backend::TriggerBackend;
use super::skill::BUILTIN_TRIGGER_SKILL_NAME;

pub(crate) const TRIGGERS_STATE_KEY: &str = "__triggers";
const TRIGGER_SKILL_GUIDANCE: &str =
    "Before creating or updating a trigger, inspect the builtin `trigger` skill with skill__get (or use skill__search) for supported rule patterns and examples.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerRuleSpec {
    pub rule: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerItem {
    pub id: u64,
    pub trigger_uuid: String,
    pub thing_uuid: String,
    pub collection_uuid: String,
    pub name: String,
    pub request: String,
    #[serde(default)]
    pub precondition: Vec<TriggerRuleSpec>,
    #[serde(default)]
    pub condition: Vec<TriggerRuleSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<bool>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_username: Option<String>,
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerUpsertRequest {
    pub(crate) id: Option<u64>,
    pub(crate) name: String,
    pub(crate) request: String,
    pub(crate) precondition: Vec<TriggerRuleSpec>,
    pub(crate) condition: Vec<TriggerRuleSpec>,
    pub(crate) enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TriggerUpsertResult {
    pub(crate) operation: String,
    pub(crate) item: TriggerItem,
}

#[derive(Debug, Deserialize)]
struct RawTriggerUpsertRequest {
    #[serde(default)]
    id: Option<u64>,
    name: String,
    request: String,
    #[serde(default)]
    precondition: Vec<TriggerRuleSpec>,
    #[serde(default)]
    condition: Vec<TriggerRuleSpec>,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

pub(crate) fn triggers_from_user_state(user_state: &serde_json::Value) -> Vec<TriggerItem> {
    match user_state.get(TRIGGERS_STATE_KEY) {
        Some(value) => serde_json::from_value(value.clone()).unwrap_or_default(),
        None => Vec::new(),
    }
}

pub(crate) fn write_triggers_to_user_state(
    user_state: &mut serde_json::Value,
    triggers: &[TriggerItem],
) {
    if !user_state.is_object() {
        *user_state = json!({});
    }
    if let Some(map) = user_state.as_object_mut() {
        map.insert(
            TRIGGERS_STATE_KEY.to_string(),
            serde_json::to_value(triggers).unwrap_or(json!([])),
        );
    }
}

fn normalize_required_text(
    tool_name: &str,
    field_name: &str,
    value: &str,
) -> Result<String, AgentError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(AgentError::tool(
            tool_name,
            format!("missing '{field_name}'"),
        ));
    }
    Ok(trimmed.to_string())
}

fn normalize_rules(
    tool_name: &str,
    field_name: &str,
    rules: Vec<TriggerRuleSpec>,
) -> Result<Vec<TriggerRuleSpec>, AgentError> {
    let mut normalized = Vec::with_capacity(rules.len());
    for (index, rule) in rules.into_iter().enumerate() {
        normalized.push(TriggerRuleSpec {
            rule: normalize_required_text(
                tool_name,
                &format!("{field_name}[{index}].rule"),
                &rule.rule,
            )?,
            description: normalize_required_text(
                tool_name,
                &format!("{field_name}[{index}].description"),
                &rule.description,
            )?,
        });
    }
    Ok(normalized)
}

pub(crate) fn parse_upsert_request(
    arguments: serde_json::Value,
) -> Result<TriggerUpsertRequest, AgentError> {
    let raw: RawTriggerUpsertRequest = serde_json::from_value(arguments).map_err(|_| {
        AgentError::tool(
            "trigger__upsert",
            "expected {id?, name, request, precondition[], condition[], enabled?}",
        )
    })?;

    let name = normalize_required_text("trigger__upsert", "name", &raw.name)?;
    let request = normalize_required_text("trigger__upsert", "request", &raw.request)?;
    let precondition = normalize_rules("trigger__upsert", "precondition", raw.precondition)?;
    let condition = normalize_rules("trigger__upsert", "condition", raw.condition)?;
    if precondition.is_empty() && condition.is_empty() {
        return Err(AgentError::tool(
            "trigger__upsert",
            "at least one rule is required across 'precondition' or 'condition'",
        ));
    }

    Ok(TriggerUpsertRequest {
        id: raw.id,
        name,
        request,
        precondition,
        condition,
        enabled: raw.enabled,
    })
}

fn preview_request(request: &str) -> String {
    const LIMIT: usize = 140;
    let trimmed = request.trim();
    let mut chars = trimmed.chars();
    let preview: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}

fn format_rule_lines(label: &str, rules: &[TriggerRuleSpec]) -> Vec<String> {
    if rules.is_empty() {
        return vec![format!("  {label}: none")];
    }

    let mut lines = vec![format!("  {label}:")];
    for rule in rules {
        lines.push(format!("    - {} ({})", rule.rule, rule.description));
    }
    lines
}

pub(crate) fn format_trigger_item(item: &TriggerItem) -> String {
    let mut lines = vec![format!("[{}] {}", item.id, item.name)];
    lines.push(format!(
        "  enabled: {}",
        if item.enabled { "yes" } else { "no" }
    ));
    lines.push(format!("  request: {}", preview_request(&item.request)));
    lines.extend(format_rule_lines("precondition", &item.precondition));
    lines.extend(format_rule_lines("condition", &item.condition));
    lines.push(format!(
        "  next_fire: {}",
        item.next_fire.as_deref().unwrap_or("none")
    ));
    if let Some(last_result) = item.last_result {
        lines.push(format!(
            "  last_result: {}",
            if last_result { "success" } else { "failure" }
        ));
    }
    lines.join("\n")
}

pub(crate) fn format_trigger_list(items: &[TriggerItem]) -> String {
    if items.is_empty() {
        return "No triggers.".to_string();
    }

    items
        .iter()
        .map(format_trigger_item)
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub struct TriggerUpsertTool {
    backend: Arc<TriggerBackend>,
}

impl TriggerUpsertTool {
    pub fn new(backend: Arc<TriggerBackend>) -> Self {
        Self { backend }
    }
}

impl Tool for TriggerUpsertTool {
    fn name(&self) -> &str {
        "trigger__upsert"
    }

    fn description(&self) -> &str {
        "Create or update one semantic trigger for the current thread. Keep the schema coarse and inspect the builtin `trigger` skill for supported rule patterns and examples."
    }

    fn extra_prompt(&self, _ctx: &ToolDefinitionContext) -> Option<String> {
        Some(format!(
            "{} Use skill__get with the exact skill name `{}` if you have not inspected it yet.",
            TRIGGER_SKILL_GUIDANCE, BUILTIN_TRIGGER_SKILL_NAME,
        ))
    }

    fn enabled(&self, _user_state: &serde_json::Value) -> bool {
        self.backend.is_configured()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "description": "Existing local trigger id to update. Omit to create a new trigger."
                },
                "name": {
                    "type": "string",
                    "description": "Short trigger name."
                },
                "request": {
                    "type": "string",
                    "description": "Full user request that should run when the trigger fires."
                },
                "precondition": {
                    "type": "array",
                    "description": "Trigger rules evaluated before the main condition. Inspect the builtin trigger skill for supported patterns.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "rule": { "type": "string" },
                            "description": { "type": "string" }
                        },
                        "required": ["rule", "description"]
                    }
                },
                "condition": {
                    "type": "array",
                    "description": "Additional trigger rules. Inspect the builtin trigger skill for supported patterns.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "rule": { "type": "string" },
                            "description": { "type": "string" }
                        },
                        "required": ["rule", "description"]
                    }
                },
                "enabled": {
                    "type": "boolean",
                    "description": "Whether the trigger should be active after the upsert. Defaults to true."
                }
            },
            "required": ["name", "request"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let request = parse_upsert_request(arguments)?;
        let result = self.backend.upsert(ctx, request).await?;
        let text = serde_json::to_string_pretty(&result).map_err(|err| {
            AgentError::other(format!("failed to serialize trigger result: {err}"))
        })?;

        Ok(ToolResult::Output(stream! {
            yield ToolOutput::text(text);
        }))
    }
}

pub struct TriggerListTool {
    backend: Arc<TriggerBackend>,
}

impl TriggerListTool {
    pub fn new(backend: Arc<TriggerBackend>) -> Self {
        Self { backend }
    }
}

impl Tool for TriggerListTool {
    fn name(&self) -> &str {
        "trigger__list"
    }

    fn description(&self) -> &str {
        "List semantic triggers for the current thread."
    }

    fn enabled(&self, _user_state: &serde_json::Value) -> bool {
        self.backend.is_configured()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(
        &self,
        _arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let items = self.backend.list(ctx).await?;
        Ok(ToolResult::Output(stream! {
            yield ToolOutput::text(format_trigger_list(&items));
        }))
    }
}

pub struct TriggerDeleteTool {
    backend: Arc<TriggerBackend>,
}

impl TriggerDeleteTool {
    pub fn new(backend: Arc<TriggerBackend>) -> Self {
        Self { backend }
    }
}

impl Tool for TriggerDeleteTool {
    fn name(&self) -> &str {
        "trigger__delete"
    }

    fn description(&self) -> &str {
        "Delete one semantic trigger by its local id."
    }

    fn enabled(&self, _user_state: &serde_json::Value) -> bool {
        self.backend.is_configured()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "description": "Local trigger id from trigger__list."
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: &ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError> {
        let id = arguments["id"]
            .as_u64()
            .ok_or_else(|| AgentError::tool("trigger__delete", "missing 'id'"))?;
        let result = self.backend.delete(ctx, id).await?;
        Ok(ToolResult::Output(stream! {
            yield ToolOutput::text(result);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_upsert_request, TriggerRuleSpec};
    use serde_json::json;

    #[test]
    fn parse_upsert_request_requires_at_least_one_rule() {
        let error = parse_upsert_request(json!({
            "name": "Morning summary",
            "request": "Send me a summary",
            "precondition": [],
            "condition": []
        }))
        .expect_err("requests without any rules should fail");

        assert!(error.to_string().contains("at least one rule"));
    }

    #[test]
    fn parse_upsert_request_normalizes_rule_text() {
        let request = parse_upsert_request(json!({
            "name": " Morning summary ",
            "request": " Send me a summary ",
            "precondition": [
                {
                    "rule": " cron('0 9 * * *') ",
                    "description": " Every day at 09:00 "
                }
            ],
            "condition": [
                {
                    "rule": " true() ",
                    "description": " Always run when due "
                }
            ]
        }))
        .expect("valid request should parse");

        assert_eq!(request.name, "Morning summary");
        assert_eq!(request.request, "Send me a summary");
        assert_eq!(
            request.precondition,
            vec![TriggerRuleSpec {
                rule: "cron('0 9 * * *')".to_string(),
                description: "Every day at 09:00".to_string(),
            }]
        );
    }
}
