use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};

use super::SearchProvider;

pub(super) struct DeepSeek;

impl SearchProvider for DeepSeek {
    fn key_env(&self) -> &'static str {
        "DEEPSEEK_API_KEY"
    }

    fn request(
        &self,
        client: &Client,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> Result<RequestBuilder, String> {
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 128 {
            return Err("DeepSeek search query must contain 1–128 Unicode characters".to_string());
        }
        if limit == 0 {
            return Err("DeepSeek search requires at least one result".to_string());
        }
        Ok(client
            .post("https://api.deepseek.com/v1/tools/search")
            .bearer_auth(api_key)
            .json(&json!({
                "query": query,
                "count": limit.min(50),
                "need_content": false
            })))
    }

    fn result_text<'a>(&self, item: &'a Value) -> &'a str {
        ["summary", "snippet"]
            .into_iter()
            .filter_map(|field| item[field].as_str())
            .find(|text| !text.trim().is_empty())
            .unwrap_or("")
    }
}
