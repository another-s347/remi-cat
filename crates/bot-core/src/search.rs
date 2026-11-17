use async_stream::stream;
use bot_runtime_core::ToolContext;
use futures::Stream;
use remi_agentloop::prelude::{AgentError, Tool, ToolOutput, ToolResult};
use remi_agentloop::types::ResumePayload;
use serde_json::json;
use std::sync::Arc;

use crate::memory::tool::{memory_agent_from_args, memory_thread_id_from_args_or_context};
use crate::memory::MemoryStore;
use crate::skill::SkillStore;

mod web;

pub(crate) use web::web_search_setup_error;
use web::{search_web, WebSearchResult};
pub use web::{WebSearchBackend, BACKEND_ENV as WEB_SEARCH_BACKEND_ENV};

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
         scope=web for web search, or scope=all for every available source. \
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
                },
                "agent": {
                    "type": "string",
                    "description": "Optional agent id for named session memory. Defaults to the current agent."
                },
                "named": {
                    "type": "string",
                    "description": "Optional named persistent sub-agent session whose thread memory should be searched."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        ctx: ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
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
            let thread_id =
                memory_thread_id_from_args_or_context(&arguments, ctx, &self.agent_id, "search")?;
            let memory_agent_id = memory_agent_from_args(&arguments, &self.agent_id, "search")?;

            for item in self
                .memory_store
                .recall(&memory_agent_id, &thread_id, &query, limit)
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
                    "id": skill.id,
                    "name": skill.name,
                    "description": skill.description,
                    "parent_id": skill.parent_id,
                    "source": skill.source,
                    "skill_file_path": skill.skill_file_path,
                    "resource_root_path": skill.resource_root_path,
                }));
            }
        }

        if scope.includes_web() {
            match search_web(&query, limit).await {
                Ok(web_results) => results.extend(web_results.into_iter().map(|item| {
                    json!({
                        "scope": "web",
                        "title": item.title,
                        "url": item.url,
                        "text": item.text,
                    })
                })),
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

/// Legacy web search entry point with a text response.
#[derive(Default)]
pub struct WebSearchTool;

impl WebSearchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. Returns titles, URLs, and content highlights. \
         Use for current events, documentation, or any topic needing fresh data."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "num_results": { "type": "integer", "description": "Max results (default 5)" }
            },
            "required": ["query"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: ToolContext,
    ) -> Result<ToolResult<impl Stream<Item = ToolOutput> + 'static>, AgentError> {
        let backend =
            web::configured_backend().map_err(|err| AgentError::tool("web_search", err))?;
        let api_key = backend
            .api_key()
            .map_err(|err| AgentError::tool("web_search", err))?;
        let query = arguments["query"]
            .as_str()
            .ok_or_else(|| AgentError::tool("web_search", "missing 'query'"))?
            .to_string();
        let limit = arguments["num_results"]
            .as_u64()
            .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
            .unwrap_or(5);

        Ok(ToolResult::Output(stream! {
            match backend.search(&api_key, &query, limit).await {
                Ok(results) => yield ToolOutput::text(format_web_results(results)),
                Err(err) => yield ToolOutput::text(format!("Error: {err}")),
            }
        }))
    }
}

fn format_web_results(results: Vec<WebSearchResult>) -> String {
    if results.is_empty() {
        return "No results found.".into();
    }
    let mut output = String::new();
    for (index, item) in results.iter().enumerate() {
        output.push_str(&format!(
            "{}. **{}**\n   {}\n   {}\n\n",
            index + 1,
            item.title,
            item.url,
            item.text.trim(),
        ));
    }
    output.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::SearchTool;
    use crate::memory::{LlmCompressor, MemoryStore};
    use crate::skill::store::{BuiltinSkill, BuiltinSkillStore, FileSkillStore};
    use bot_runtime_core::ToolContext;
    use futures::StreamExt;
    use remi_agentloop::prelude::{Message, Tool, ToolOutput, ToolResult};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn memory_store(data_dir: PathBuf) -> Arc<MemoryStore> {
        Arc::new(MemoryStore {
            memory_dir: data_dir.join("memory"),
            data_dir,
            agent_md_path: None,
            compressor: LlmCompressor::new(
                "test-key".to_string(),
                None,
                "gpt-4o-mini".to_string(),
                128_000,
                4096,
                serde_json::Map::new(),
            ),
            short_term_tokens: 8192,
            auto_compress: false,
            memory_days: 7,
        })
    }

    fn tool_context(thread_id: Option<&str>) -> ToolContext {
        ToolContext::with_ids(
            serde_json::from_value(serde_json::json!("fallback-thread"))
                .expect("thread id should deserialize"),
            serde_json::from_value(serde_json::json!("test-run"))
                .expect("run id should deserialize"),
            bot_runtime_core::ChatCtxState {
                metadata: thread_id.map(|id| json!({ "thread_id": id })),
                ..bot_runtime_core::ChatCtxState::default()
            },
        )
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
                tool_context(Some("thread-1")),
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
                tool_context(Some("thread-1")),
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
    async fn memory_scope_can_target_named_subagent_session() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = memory_store(tmp.path().to_path_buf());
        memory
            .save_turn(
                "subagent:coder:feature_a",
                vec![Message::user("alpha exists in named session")],
            )
            .await
            .unwrap();
        memory
            .save_turn(
                "thread-1",
                vec![Message::user("beta exists in current thread")],
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
                json!({
                    "query": "alpha",
                    "scope": "memory",
                    "agent": "coder",
                    "named": "feature_a"
                }),
                None,
                tool_context(Some("thread-1")),
            )
            .await
            .unwrap(),
        )
        .await;
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let results = value["results"].as_array().unwrap();
        assert!(results.iter().any(|item| item["snippet"]
            .as_str()
            .is_some_and(|snippet| snippet.contains("alpha"))));
        assert!(!results.iter().any(|item| item["snippet"]
            .as_str()
            .is_some_and(|snippet| snippet.contains("beta"))));
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
            tool_context(Some("thread-1")),
        )
        .await
        {
            Ok(_) => panic!("invalid scope should fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("invalid 'scope'"));
    }
}
