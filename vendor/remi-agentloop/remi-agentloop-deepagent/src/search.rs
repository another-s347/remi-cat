//! Exa web search tool.
//!
//! Calls the Exa Search API (`https://api.exa.ai/search`) and returns
//! formatted results with title, URL, and content highlights.
//!
//! Set `EXA_API_KEY` env var or pass the key directly via [`ExaSearchTool::new`].

use async_stream::stream;
use futures::Stream;

use remi_core::error::AgentError;
use remi_core::tool::{Tool, ToolContext, ToolOutput, ToolResult};
use remi_core::types::ResumePayload;

const EXA_API_URL: &str = "https://api.exa.ai/search";

/// Web search via the Exa Search API.
///
/// # Usage
/// ```no_run
/// use remi_deepagent::ExaSearchTool;
///
/// let tool = ExaSearchTool::new("your-exa-api-key");
/// // or from env var:
/// let tool = ExaSearchTool::from_env().expect("EXA_API_KEY not set");
/// ```
pub struct ExaSearchTool {
    api_key: String,
    num_results: usize,
    search_type: String, // "auto" | "neural" | "fast" | "deep"
}

impl ExaSearchTool {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            num_results: 5,
            search_type: "auto".to_string(),
        }
    }

    /// Read API key from `EXA_API_KEY` env var.
    pub fn from_env() -> Option<Self> {
        std::env::var("EXA_API_KEY").ok().map(Self::new)
    }

    pub fn num_results(mut self, n: usize) -> Self {
        self.num_results = n;
        self
    }

    /// Set the Exa search type: `"auto"` (default), `"neural"`, `"fast"`, `"deep"`.
    pub fn search_type(mut self, t: impl Into<String>) -> Self {
        self.search_type = t.into();
        self
    }
}

impl Tool for ExaSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web using Exa. \
         Returns relevant results with titles, URLs, and content highlights. \
         Use for current events, documentation look-ups, or any topic requiring fresh web data."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query string"
                },
                "num_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default 5)",
                    "default": 5
                }
            },
            "required": ["query"]
        })
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
        _resume: Option<ResumePayload>,
        _ctx: &ToolContext,
    ) -> impl std::future::Future<Output = Result<ToolResult<impl Stream<Item = ToolOutput>>, AgentError>>
    {
        let api_key = self.api_key.clone();
        let default_num = self.num_results;
        let search_type = self.search_type.clone();

        async move {
            let query = arguments["query"]
                .as_str()
                .ok_or_else(|| AgentError::tool("web_search", "missing 'query'"))?
                .to_string();

            let num_results = arguments["num_results"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(default_num);

            Ok(ToolResult::Output(stream! {
                let client = reqwest::Client::new();
                let body = serde_json::json!({
                    "query": query,
                    "type": search_type,
                    "numResults": num_results,
                    "contents": {
                        "text": { "maxCharacters": 2000 },
                        "highlights": { "maxCharacters": 1000 }
                    }
                });

                let resp = match client
                    .post(EXA_API_URL)
                    .header("x-api-key", &api_key)
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        yield ToolOutput::text(format!("error contacting Exa: {e}"));
                        return;
                    }
                };

                let status = resp.status();
                let text = match resp.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        yield ToolOutput::text(format!("error reading Exa response: {e}"));
                        return;
                    }
                };

                if !status.is_success() {
                    yield ToolOutput::text(format!("Exa API error {}: {}", status, text));
                    return;
                }

                let json: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(e) => {
                        yield ToolOutput::text(format!("failed to parse Exa response: {e}"));
                        return;
                    }
                };

                let mut output = String::new();

                if let Some(results) = json["results"].as_array() {
                    for (i, result) in results.iter().enumerate() {
                        let title = result["title"].as_str().unwrap_or("(no title)");
                        let url   = result["url"].as_str().unwrap_or("");
                        let date  = result["publishedDate"].as_str().unwrap_or("");

                        output.push_str(&format!("{}. **{}**\n   {}\n", i + 1, title, url));
                        if !date.is_empty() {
                            let date_short: String = date.chars().take(10).collect();
                            output.push_str(&format!("   Published: {}\n", date_short));
                        }

                        // Prefer highlights (more concise), fall back to text
                        let snippet = result["highlights"]
                            .as_array()
                            .and_then(|h| h.first())
                            .and_then(|h| h.as_str())
                            .or_else(|| result["text"].as_str())
                            .unwrap_or("");

                        if !snippet.is_empty() {
                            let trimmed: String = snippet.chars().take(400).collect();
                            output.push_str(&format!("   {}\n", trimmed));
                        }
                        output.push('\n');
                    }
                }

                if output.is_empty() {
                    output = "No results found.".to_string();
                }

                yield ToolOutput::text(output);
            }))
        }
    }
}

/// Type alias for backward compatibility.
pub type TavilySearchTool = ExaSearchTool;
