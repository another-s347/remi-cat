use reqwest::Client;
use serde_json::{json, Value};

use super::{parse_response, preview, require_api_key, WebSearchBackend};

#[test]
fn backend_selection_uses_provider_specific_credentials() {
    assert_eq!(WebSearchBackend::default(), WebSearchBackend::Exa);
    for (name, key_env) in [("exa", "EXA_API_KEY"), ("deepseek", "DEEPSEEK_API_KEY")] {
        let backend: WebSearchBackend = name.parse().unwrap();
        assert_eq!(backend.as_str(), name);
        assert_eq!(backend.provider().key_env(), key_env);
        assert!(require_api_key(key_env, Some("  ".into()))
            .unwrap_err()
            .contains(key_env));
    }
    assert!("unknown".parse::<WebSearchBackend>().is_err());
}

#[test]
fn exa_request_preserves_search_options() {
    let request = WebSearchBackend::Exa
        .provider()
        .request(&Client::new(), "test-key", "rust", 5)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(request.method(), reqwest::Method::POST);
    assert_eq!(request.url().as_str(), "https://api.exa.ai/search");
    assert_eq!(request.headers()["x-api-key"], "test-key");
    assert_eq!(
        serde_json::from_slice::<Value>(request.body().unwrap().as_bytes().unwrap()).unwrap(),
        json!({"query": "rust", "numResults": 5, "contents": {"text": {"maxCharacters": 1000}}})
    );
}

#[test]
fn deepseek_request_uses_bearer_auth_and_bounded_count() {
    let provider = WebSearchBackend::DeepSeek.provider();
    let client = Client::new();
    let query = "搜".repeat(128);
    let request = provider
        .request(&client, "test-key", &query, 100)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(request.method(), reqwest::Method::POST);
    assert_eq!(
        request.url().as_str(),
        "https://api.deepseek.com/v1/tools/search"
    );
    assert_eq!(request.headers()["authorization"], "Bearer test-key");
    assert_eq!(
        serde_json::from_slice::<Value>(request.body().unwrap().as_bytes().unwrap()).unwrap(),
        json!({"query": query, "count": 50, "need_content": false})
    );
    assert!(provider
        .request(&client, "test-key", &"搜".repeat(129), 5)
        .is_err());
    assert!(provider.request(&client, "test-key", " ", 5).is_err());
    assert!(provider.request(&client, "test-key", "rust", 0).is_err());
}

#[test]
fn exa_results_are_parsed() {
    let results = parse_response(
        WebSearchBackend::Exa.provider(),
        200,
        r#"{"results":[{"title":"Example","url":"https://example.com","text":"Body"}]}"#,
        5,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Example");
    assert_eq!(results[0].url, "https://example.com");
    assert_eq!(results[0].text, "Body");
}

#[test]
fn deepseek_results_use_summary_with_snippet_fallback() {
    let results = parse_response(
        WebSearchBackend::DeepSeek.provider(),
        200,
        r#"{"results":[
            {"title":"Example","url":"https://example.com","summary":"Long","snippet":"Short"},
            {"title":"Other","summary":"  ","snippet":"Fallback"},
            {"title":"Extra","summary":"Outside the limit"}
        ]}"#,
        2,
    )
    .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Example");
    assert_eq!(results[0].url, "https://example.com");
    assert_eq!(results[0].text, "Long");
    assert_eq!(results[1].text, "Fallback");
    assert!(parse_response(
        WebSearchBackend::DeepSeek.provider(),
        200,
        r#"{"results":[]}"#,
        5
    )
    .unwrap()
    .is_empty());
}

#[test]
fn http_errors_are_reported_instead_of_empty_results() {
    for (backend, status, body, message) in [
        (
            WebSearchBackend::Exa,
            401,
            r#"{"message":"Invalid API key"}"#,
            "Invalid API key",
        ),
        (
            WebSearchBackend::DeepSeek,
            402,
            r#"{"error":{"message":"Insufficient Balance"}}"#,
            "Insufficient Balance",
        ),
        (
            WebSearchBackend::DeepSeek,
            429,
            "Too Many Requests",
            "Too Many Requests",
        ),
    ] {
        let error = parse_response(backend.provider(), status, body, 5)
            .err()
            .unwrap();
        assert!(error.contains(&format!("HTTP {status}")), "{error}");
        assert!(error.contains(message), "{error}");
    }
}

#[test]
fn missing_results_array_is_reported() {
    let error = parse_response(
        WebSearchBackend::Exa.provider(),
        200,
        r#"{"requestId":"req_1","error":"quota"}"#,
        5,
    )
    .err()
    .unwrap();
    assert!(error.contains("missing results array"), "{error}");
    assert!(error.contains("requestId"), "{error}");
}

#[test]
fn preview_truncates_on_utf8_boundary() {
    let text = format!("{}🙂tail", "a".repeat(499));
    let value = preview(&text);
    assert!(value.ends_with("..."));
    assert!(!value.contains("tail"));
}
