use async_stream::stream;
use futures::Stream;
use remi_agentloop::prelude::{AgentError, Tool, ToolContext, ToolOutput, ToolResult};
use remi_agentloop::types::ResumePayload;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

use crate::memory::MemoryStore;
use crate::skill::SkillStore;

const EXA_API_URL: &str = "https://api.exa.ai/search";
const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchScope {
    Local,
    Memory,
    Skills,
    Web,
    All,
}

impl SearchScope {
    fn parse(raw: Option<&str>) -> Result<Self, AgentError> {
        let Some(raw) = raw else {
            return Ok(Self::Local);
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "local" => Ok(Self::Local),
            "memory" | "memories" => Ok(Self::Memory),
            "skill" | "skills" => Ok(Self::Skills),
            "web" | "internet" => Ok(Self::Web),
            "all" => Ok(Self::All),
            other => Err(AgentError::tool(
                "search",
                format!(
                    "invalid 'scope' {other:?}; expected one of: local, memory, skills, web, all"
                ),
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Memory => "memory",
            Self::Skills => "skills",
            Self::Web => "web",
            Self::All => "all",
        }
    }

    fn includes_memory(self) -> bool {
        matches!(self, Self::Local | Self::Memory | Self::All)
    }

    fn includes_skills(self) -> bool {
        matches!(self, Self::Local | Self::Skills | Self::All)
    }

    fn includes_web(self) -> bool {
        matches!(self, Self::Web | Self::All)
    }
}

pub struct SearchTool<S> {
    pub skill_store: Arc<S>,
    pub memory_store: Arc<MemoryStore>,
    pub agent_id: String,
}

impl<S: SkillStore + 'static> Tool for SearchTool<S> {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "Unified search tool. By default scope=local searches memory and skills. \
         Set scope=memory for named/thread memory, scope=skills for local skills, \
         scope=web for Exa web search, or scope=all for every available source. \
         Use scope=memory before answering questions about prior conversations, saved user facts, \
         preferences, or details that are not visible in the current short context."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query or keywords"
                },
                "scope": {
                    "type": "string",
                    "enum": ["local", "memory", "skills", "web", "all"],
                    "description": "Optional search range. Default local searches memory and skills."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results per scope. Default 8, maximum 20."
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
            .ok_or_else(|| AgentError::tool("search", "missing 'query'"))?
            .trim()
            .to_string();
        if query.is_empty() {
            return Err(AgentError::tool("search", "'query' must not be empty"));
        }

        let scope = SearchScope::parse(arguments["scope"].as_str())?;
        let limit = arguments["limit"]
            .as_u64()
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);

        let mut results = Vec::new();
        let mut errors = Vec::new();

        if scope.includes_memory() {
            let thread_id = ctx
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("thread_id"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .or_else(|| ctx.thread_id.as_ref().map(ToString::to_string))
                .ok_or_else(|| AgentError::tool("search", "missing metadata.thread_id"))?;

            for item in self
                .memory_store
                .recall(&self.agent_id, &thread_id, &query, limit)
                .await?
            {
                results.push(json!({
                    "scope": "memory",
                    "source": item.source,
                    "name": item.name,
                    "uuid": item.uuid,
                    "timestamp": item.timestamp,
                    "preview": item.preview,
                    "snippet": item.snippet,
                    "score": item.score,
                }));
            }
        }

        if scope.includes_skills() {
            for skill in self
                .skill_store
                .search(&query)
                .await?
                .into_iter()
                .take(limit)
            {
                results.push(json!({
                    "scope": "skills",
                    "name": skill.name,
                    "description": skill.description,
                }));
            }
        }

        if scope.includes_web() {
            match search_web(&query, limit).await {
                Ok(web_results) => results.extend(web_results),
                Err(err) => errors.push(err),
            }
        }

        let output = json!({
            "query": query,
            "scope": scope.label(),
            "default_scope": "local",
            "limit": limit,
            "results": results,
            "errors": errors,
        });

        Ok(ToolResult::Output(stream! {
            yield ToolOutput::text(output.to_string());
        }))
    }
}

async fn search_web(query: &str, limit: usize) -> Result<Vec<serde_json::Value>, String> {
    let api_key = std::env::var("EXA_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| "EXA_API_KEY is not set; web search is unavailable".to_string())?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| format!("failed to build web search client: {err}"))?;
    let body = json!({
        "query": query,
        "numResults": limit,
        "contents": { "text": { "maxCharacters": 1000 } }
    });

    let response = client
        .post(EXA_API_URL)
        .header("x-api-key", api_key)
        .json(&body)
        .send()
        .await
        .map_err(|err| format!("web search request failed: {err}"))?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|err| format!("web search response read failed: {err}"))?;
    parse_exa_search_response(status, &body, limit)
}

fn parse_exa_search_response(
    status: u16,
    body: &str,
    limit: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let data: serde_json::Value = serde_json::from_str(body).map_err(|err| {
        format!(
            "web search response parse failed: {err}; body={}",
            preview(body)
        )
    })?;
    if !(200..300).contains(&status) {
        return Err(format!(
            "web search HTTP {status}: {}",
            exa_error_message(&data).unwrap_or_else(|| preview(body))
        ));
    }
    let mut results = Vec::new();
    let items = data["results"].as_array().ok_or_else(|| {
        let keys = data
            .as_object()
            .map(|object| object.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| data.to_string());
        format!("web search response missing results array; response keys: {keys}")
    })?;
    for item in items.iter().take(limit) {
        results.push(json!({
            "scope": "web",
            "title": item["title"].as_str().unwrap_or("(no title)"),
            "url": item["url"].as_str().unwrap_or(""),
            "text": item["text"].as_str().unwrap_or(""),
        }));
    }
    Ok(results)
}

fn exa_error_message(data: &serde_json::Value) -> Option<String> {
    for key in ["message", "error", "detail"] {
        let Some(value) = data.get(key) else {
            continue;
        };
        if let Some(text) = value.as_str() {
            return Some(text.to_string());
        }
        if !value.is_null() {
            return Some(value.to_string());
        }
    }
    None
}

fn preview(text: &str) -> String {
    const MAX: usize = 500;
    if text.len() <= MAX {
        return text.to_string();
    }
    format!("{}...", &text[..MAX])
}

#[cfg(test)]
mod tests {
    use super::{parse_exa_search_response, SearchTool};
    use crate::memory::{LlmCompressor, MemoryStore};
    use crate::skill::store::{BuiltinSkill, BuiltinSkillStore, FileSkillStore};
    use futures::StreamExt;
    use remi_agentloop::prelude::{AgentConfig, Tool, ToolContext, ToolOutput, ToolResult};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{Arc, RwLock};

    fn memory_store(data_dir: PathBuf) -> Arc<MemoryStore> {
        Arc::new(MemoryStore {
            data_dir,
            agent_md_path: None,
            compressor: LlmCompressor::new(
                "test-key".to_string(),
                None,
                "gpt-4o-mini".to_string(),
                4096,
                serde_json::Map::new(),
            ),
            short_term_tokens: 8192,
            auto_compress: false,
            memory_days: 7,
        })
    }

    fn tool_context(thread_id: Option<&str>) -> ToolContext {
        ToolContext {
            config: AgentConfig::default(),
            thread_id: Some(
                serde_json::from_value(serde_json::json!("fallback-thread"))
                    .expect("thread id should deserialize"),
            ),
            run_id: serde_json::from_value(serde_json::json!("test-run"))
                .expect("run id should deserialize"),
            metadata: thread_id.map(|id| json!({ "thread_id": id })),
            user_state: Arc::new(RwLock::new(serde_json::Value::Null)),
        }
    }

    async fn collect_text(result: ToolResult<impl futures::Stream<Item = ToolOutput>>) -> String {
        match result {
            ToolResult::Interrupt(_) => "interrupted".to_string(),
            ToolResult::Output(output) => {
                let mut output = std::pin::pin!(output);
                let mut text = String::new();
                while let Some(item) = output.next().await {
                    if let ToolOutput::Result(content) = item {
                        text = content.text_content();
                    }
                }
                text
            }
        }
    }

    fn skill_store() -> Arc<BuiltinSkillStore<FileSkillStore>> {
        Arc::new(BuiltinSkillStore::new(
            FileSkillStore::with_roots([]),
            [BuiltinSkill {
                name: "aurora-skill",
                description: "Teal aurora workflow",
                content: "---\nname: aurora-skill\ndescription: Teal aurora workflow\n---\n\nBody"
                    .to_string(),
            }],
        ))
    }

    #[tokio::test]
    async fn default_local_scope_searches_memory_and_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = memory_store(tmp.path().to_path_buf());
        memory
            .upsert_named_memory(
                "default",
                "aurora-note",
                "CLI smoke favorite color is teal and recall keyword is aurora.",
            )
            .await
            .unwrap();
        let tool = SearchTool {
            skill_store: skill_store(),
            memory_store: memory,
            agent_id: "default".to_string(),
        };

        let text = collect_text(
            <SearchTool<_> as Tool>::execute(
                &tool,
                json!({ "query": "aurora teal" }),
                None,
                &tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["scope"], "local");
        assert_eq!(value["default_scope"], "local");
        let results = value["results"].as_array().unwrap();
        assert!(results.iter().any(|item| item["scope"] == "memory"));
        assert!(results.iter().any(|item| item["scope"] == "skills"));
    }

    #[tokio::test]
    async fn scope_filters_and_limit_are_applied() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = memory_store(tmp.path().to_path_buf());
        memory
            .upsert_named_memory("default", "aurora-note", "aurora teal memory")
            .await
            .unwrap();
        let tool = SearchTool {
            skill_store: skill_store(),
            memory_store: memory,
            agent_id: "default".to_string(),
        };

        let text = collect_text(
            <SearchTool<_> as Tool>::execute(
                &tool,
                json!({ "query": "aurora", "scope": "memory", "limit": 99 }),
                None,
                &tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["limit"], 20);
        assert!(value["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["scope"] == "memory"));
    }

    #[tokio::test]
    async fn web_scope_reports_missing_exa_key_without_thread_id() {
        std::env::remove_var("EXA_API_KEY");
        let tmp = tempfile::tempdir().unwrap();
        let tool = SearchTool {
            skill_store: skill_store(),
            memory_store: memory_store(tmp.path().to_path_buf()),
            agent_id: "default".to_string(),
        };

        let text = collect_text(
            <SearchTool<_> as Tool>::execute(
                &tool,
                json!({ "query": "aurora", "scope": "web" }),
                None,
                &tool_context(None),
            )
            .await
            .unwrap(),
        )
        .await;
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["scope"], "web");
        assert!(value["errors"][0]
            .as_str()
            .unwrap()
            .contains("EXA_API_KEY is not set"));
    }

    #[test]
    fn exa_http_error_is_reported_instead_of_empty_results() {
        let error = parse_exa_search_response(401, r#"{"message":"Invalid API key"}"#, 5)
            .expect_err("non-2xx Exa responses should fail");

        assert!(error.contains("HTTP 401"), "{error}");
        assert!(error.contains("Invalid API key"), "{error}");
    }

    #[test]
    fn exa_missing_results_array_is_reported() {
        let error = parse_exa_search_response(200, r#"{"requestId":"req_1","error":"quota"}"#, 5)
            .expect_err("missing results should fail clearly");

        assert!(error.contains("missing results array"), "{error}");
        assert!(error.contains("requestId"), "{error}");
    }

    #[test]
    fn exa_results_are_parsed() {
        let results = parse_exa_search_response(
            200,
            r#"{"results":[{"title":"Example","url":"https://example.com","text":"Body"}]}"#,
            5,
        )
        .expect("valid Exa response should parse");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["scope"], "web");
        assert_eq!(results[0]["title"], "Example");
    }

    #[tokio::test]
    async fn invalid_scope_returns_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = SearchTool {
            skill_store: skill_store(),
            memory_store: memory_store(tmp.path().to_path_buf()),
            agent_id: "default".to_string(),
        };

        let err = match <SearchTool<_> as Tool>::execute(
            &tool,
            json!({ "query": "aurora", "scope": "bad" }),
            None,
            &tool_context(Some("thread-1")),
        )
        .await
        {
            Ok(_) => panic!("invalid scope should fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("invalid 'scope'"));
    }
}
