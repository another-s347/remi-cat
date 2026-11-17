mod deepseek;
mod exa;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use reqwest::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const BACKEND_ENV: &str = "REMI_SEARCH_BACKEND";

/// The provider used by both built-in web search entry points.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WebSearchBackend {
    #[default]
    Exa,
    DeepSeek,
}

impl WebSearchBackend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exa => "exa",
            Self::DeepSeek => "deepseek",
        }
    }

    fn provider(self) -> &'static dyn SearchProvider {
        match self {
            Self::Exa => &exa::Exa,
            Self::DeepSeek => &deepseek::DeepSeek,
        }
    }
}

impl std::str::FromStr for WebSearchBackend {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "exa" => Ok(Self::Exa),
            "deepseek" => Ok(Self::DeepSeek),
            _ => Err(format!(
                "invalid search backend {value:?}; expected exa or deepseek"
            )),
        }
    }
}

pub(super) struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub text: String,
}

pub(super) trait SearchProvider: Sync {
    fn key_env(&self) -> &'static str;

    fn request(
        &self,
        client: &Client,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> Result<RequestBuilder, String>;

    fn result_text<'a>(&self, item: &'a Value) -> &'a str;
}

pub(super) fn configured_backend() -> Result<WebSearchBackend, String> {
    match std::env::var(BACKEND_ENV) {
        Ok(value) => value.parse().map_err(|err| format!("{BACKEND_ENV}: {err}")),
        Err(std::env::VarError::NotPresent) => Ok(WebSearchBackend::default()),
        Err(err) => Err(format!("cannot read {BACKEND_ENV}: {err}")),
    }
}

pub(crate) fn web_search_setup_error() -> Option<String> {
    configured_backend()
        .and_then(WebSearchBackend::api_key)
        .err()
}

pub(super) async fn search_web(query: &str, limit: usize) -> Result<Vec<WebSearchResult>, String> {
    let backend = configured_backend()?;
    let api_key = backend.api_key()?;
    backend.search(&api_key, query, limit).await
}

impl WebSearchBackend {
    pub(super) fn api_key(self) -> Result<String, String> {
        let key_env = self.provider().key_env();
        require_api_key(key_env, std::env::var(key_env).ok())
    }

    pub(super) async fn search(
        self,
        api_key: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<WebSearchResult>, String> {
        let provider = self.provider();
        let client = search_client()?;
        let request = provider.request(client, api_key, query, limit)?;
        let started = Instant::now();
        tracing::info!(
            backend = self.as_str(),
            num_results = limit,
            "web_search.start"
        );

        let result = async {
            let response = request
                .send()
                .await
                .map_err(|err| format!("web search request failed: {err}"))?;
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .map_err(|err| format!("web search response read failed: {err}"))?;
            parse_response(provider, status, &body, limit)
        }
        .await;

        let elapsed_ms = started.elapsed().as_millis() as u64;
        match &result {
            Ok(results) => tracing::info!(
                backend = self.as_str(),
                results = results.len(),
                elapsed_ms,
                "web_search.completed"
            ),
            Err(err) => tracing::warn!(
                backend = self.as_str(),
                error = %err,
                elapsed_ms,
                "web_search.failed"
            ),
        }
        result
    }
}

fn require_api_key(key_env: &str, value: Option<String>) -> Result<String, String> {
    value.filter(|key| !key.trim().is_empty()).ok_or_else(|| {
        format!("{key_env} is not set; use `/secrets set {key_env} <key>` to enable web search")
    })
}

fn search_client() -> Result<&'static Client, String> {
    static CLIENT: OnceLock<Result<Client, reqwest::Error>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()
        })
        .as_ref()
        .map_err(|err| format!("failed to build web search client: {err}"))
}

fn parse_response(
    provider: &dyn SearchProvider,
    status: u16,
    body: &str,
    limit: usize,
) -> Result<Vec<WebSearchResult>, String> {
    if !(200..300).contains(&status) {
        let error = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|data| error_message(&data));
        return Err(format!(
            "web search HTTP {status}: {}",
            preview(error.as_deref().unwrap_or(body))
        ));
    }

    let data: Value = serde_json::from_str(body).map_err(|err| {
        format!(
            "web search response parse failed: {err}; body={}",
            preview(body)
        )
    })?;
    let items = data["results"].as_array().ok_or_else(|| {
        let keys = data
            .as_object()
            .map(|object| object.keys().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| preview(body));
        format!("web search response missing results array; response keys: {keys}")
    })?;

    Ok(items
        .iter()
        .take(limit)
        .map(|item| WebSearchResult {
            title: item["title"].as_str().unwrap_or("(no title)").to_string(),
            url: item["url"].as_str().unwrap_or("").to_string(),
            text: provider.result_text(item).to_string(),
        })
        .collect())
}

fn error_message(data: &Value) -> Option<String> {
    for key in ["message", "error", "detail"] {
        let Some(value) = data.get(key).filter(|value| !value.is_null()) else {
            continue;
        };
        return Some(match value.as_str().or_else(|| value["message"].as_str()) {
            Some(message) => message.to_string(),
            None => value.to_string(),
        });
    }
    None
}

fn preview(text: &str) -> String {
    const MAX_BYTES: usize = 500;
    if text.len() <= MAX_BYTES {
        return text.to_string();
    }
    let mut end = MAX_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests;
