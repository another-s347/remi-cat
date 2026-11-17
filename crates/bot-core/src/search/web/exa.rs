use reqwest::{Client, RequestBuilder};
use serde_json::{json, Value};

use super::SearchProvider;

pub(super) struct Exa;

impl SearchProvider for Exa {
    fn key_env(&self) -> &'static str {
        "EXA_API_KEY"
    }

    fn request(
        &self,
        client: &Client,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> Result<RequestBuilder, String> {
        Ok(client
            .post("https://api.exa.ai/search")
            .header("x-api-key", api_key)
            .json(&json!({
                "query": query,
                "numResults": limit,
                "contents": { "text": { "maxCharacters": 1000 } }
            })))
    }

    fn result_text<'a>(&self, item: &'a Value) -> &'a str {
        item["text"].as_str().unwrap_or("")
    }
}
